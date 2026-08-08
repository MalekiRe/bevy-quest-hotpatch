//! Patch engine: turns a source change into a subsecond JumpTable for the
//! running Android app, using whisker's hotpatch machinery (thin rebuild of
//! only the tip crate, linked via NDK clang against the live app's symbols).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use subsecond_types::JumpTable;
use whisker_dev_server::hotpatch::{
    CapturedLinkerInvocation, CapturedRustcInvocation, HotpatchModuleCache, LinkerOs,
    build_jump_table, build_link_plan, build_obj_plan, compute_needed_symbols,
    load_captured_args, load_captured_linker_args,
    parse_symbol_table, run_link_plan, run_obj_plan,
};

/// The tip crate that owns the hot-patchable code (must match `[lib] name`).
pub const PACKAGE: &str = "quest_hotpatch_app";

/// Where the engine keeps its scratch artifacts.
pub fn scratch_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join("target/.questhotpatch")
}

pub struct PatchSession {
    pub workspace_root: PathBuf,
    pub original_binary: PathBuf,
    pub real_linker: PathBuf,
    pub rustc_path: PathBuf,
    pub cache: HotpatchModuleCache,
    pub captured_rustc: HashMap<String, CapturedRustcInvocation>,
    pub captured_linker: HashMap<String, CapturedLinkerInvocation>,
}

impl PatchSession {
    /// Load state produced by the fat build (`build` subcommand / `capture_fat_build`):
    /// captured rustc+linker invocations + the original APK `.so` as the baseline.
    pub fn load(workspace_root: &Path, original_binary: &Path, real_linker: &Path) -> Result<Self> {
        let scratch = scratch_dir(workspace_root);
        let captured_rustc = load_captured_args(&scratch.join("rustc"), Some("aarch64-linux-android"))
            .context("load captured rustc args")?;
        let captured_linker =
            load_captured_linker_args(&scratch.join("linker")).context("load captured linker args")?;
        let cache = HotpatchModuleCache::from_path(original_binary)
            .with_context(|| format!("parse original binary {}", original_binary.display()))?;
        Ok(Self {
            workspace_root: workspace_root.to_path_buf(),
            original_binary: original_binary.to_path_buf(),
            real_linker: real_linker.to_path_buf(),
            rustc_path: PathBuf::from(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into())),
            cache,
            captured_rustc,
            captured_linker,
        })
    }

    pub fn is_ready(&self) -> bool {
        self.cache.aslr_reference != 0
            && self.captured_rustc.contains_key(PACKAGE)
            && !self.captured_linker.is_empty()
    }

    /// Build one patch: fresh `.o` of the tip crate -> stub-object of host
    /// symbol jumps -> NDK link -> diff -> JumpTable.
    ///
    /// `device_aslr` = the `aslr_reference()` the app reported over the wire.
    pub async fn build_patch(&self, device_aslr: u64) -> Result<(JumpTable, PathBuf)> {
        let scratch = scratch_dir(&self.workspace_root);
        let objects = scratch.join("objects");
        let patches = scratch.join("patches");
        std::fs::create_dir_all(&objects)?;
        std::fs::create_dir_all(&patches)?;

        // 1. Rebuild ONLY the tip crate as an object file (thin rebuild).
        let captured_rustc = self
            .captured_rustc
            .get(PACKAGE)
            .context("no captured rustc invocation for the tip crate")?;
        let obj_plan = build_obj_plan(captured_rustc, &objects);
        let object_path = run_obj_plan(&obj_plan, &self.rustc_path, &self.workspace_root).await?;

        // 2. Resolve the symbols the patch references against the live app,
        //    via a synthesized stub object (whisker's approach).
        let stub_bytes = build_stub_full(
            &self.cache,
            &object_path,
            device_aslr,
        )?;
        let stub_path = objects.join("stub.o");
        std::fs::write(&stub_path, &stub_bytes)?;

        // 3. Link into a patch shared library with the NDK clang, using the
        //    captured linker invocation as the argument template.
        let captured_linker = self
            .captured_linker
            .values()
            .next()
            .context("no captured linker invocation")?;
        let patch_path = patches.join("libquest_hotpatch_app_patch.so");
        let link_plan = build_link_plan(
            &captured_linker.args,
            &object_path,
            &patch_path,
            LinkerOs::Linux,
            &[stub_path],
            &[],
        );
        run_link_plan(&link_plan, &self.real_linker, &self.workspace_root).await?;

        // 4. Diff original vs patch symbol tables and build the JumpTable.
        let new_syms = parse_symbol_table(&patch_path).context("parse patch symbols")?;
        // Anchor symbol the runtime + whisker use to pin ASLR; must exist in
        // BOTH the original binary (exported by our app) and the patch.
        let new_base_address = new_syms
            .by_name
            .get("whisker_aslr_anchor")
            .map(|s| s.address)
            .unwrap_or(0);
        // Subsecond's apply_patch() anchors ASLR on `main` (aslr_reference() =
        // dlsym("main")), so the JumpTable must use main's build-time address for
        // BOTH aslr_reference and new_base_address — matching the patch .o's.
        let aslr_ref_main = self
            .cache
            .symbols
            .by_name
            .get("main")
            .map(|s| s.address)
            .unwrap_or(self.cache.aslr_reference);
        let new_base_main = new_syms
            .by_name
            .get("main")
            .map(|s| s.address)
            .unwrap_or(new_base_address);
        let mut plan = build_jump_table(
            &self.cache.symbols,
            &new_syms,
            patch_path.clone(),
            aslr_ref_main,
            new_base_main,
        );

        // SAFETY-CRITICAL FILTER: remap ONLY the intended hot functions.
        // Redirecting everything (e.g. bevy systems whose dispatchers use a
        // different calling convention, or dep-adjacent tip functions running on
        // render/compute threads) makes the app execute wrong code and crash.
        // The demo's hot boundary is `desired_color`; every other entry falls
        // back to the deployed (old) code, which stays correct.
        const HOT_FUNCS: &[&str] = &["desired_color"];
        let old_name_of: std::collections::HashMap<u64, &str> = self
            .cache
            .symbols
            .by_name
            .iter()
            .filter(|(_, s)| s.address != 0)
            .map(|(n, s)| (s.address, n.as_str()))
            .collect();
        let before = plan.table.map.len();
        plan.table.map = plan
            .table
            .map
            .into_iter()
            .filter(|(k, _)| {
                old_name_of
                    .get(k)
                    .map(|name| HOT_FUNCS.iter().any(|h| name.contains(h)))
                    .unwrap_or(false)
            })
            .collect();
        tracing::info!(before = before, after = plan.table.map.len(), "jump table filtered");
        // DIAGNOSTIC: is paint_cube present in old/new and mapped old->new?
        for pat in ["paint_cube", "rotate"] {
            let old_present = self.cache.symbols.by_name.iter().any(|(n, s)| n.contains(pat) && s.address != 0);
            let new_present = new_syms.by_name.iter().any(|(n, s)| n.contains(pat) && s.address != 0);
            let mapped = self.cache.symbols.by_name.iter()
                .filter(|(n, s)| n.contains(pat) && s.address != 0)
                .any(|(_, s)| plan.table.map.contains_key(&s.address));
            tracing::info!(name = %pat, old_present = old_present, new_present = new_present, mapped_oldaddr_in_table = mapped, "DIAG");
        }
        tracing::info!(added = ?plan.report.added, removed = ?plan.report.removed, "patch diff");
        Ok((plan.table, patch_path))
    }
}


/// Full stub-object builder: trampolines for changed CODE symbols plus
/// absolute-address definitions for host DATA symbols (statics/globals like
/// `log::MAX_LOG_LEVEL_FILTER`), which upstream whisker skips. We classify
/// code vs data ourselves from the original ELF's section flags.
fn build_stub_full(cache: &HotpatchModuleCache, patch_obj: &std::path::Path, device_aslr: u64) -> Result<Vec<u8>> {
    use object::{Object as _, ObjectSymbol as _};
    use object::write::{Object, StandardSection, Symbol, SymbolSection};
    use object::{Architecture, BinaryFormat, Endianness, SymbolFlags, SymbolKind as ObjKind, SymbolScope};

    let aslr_offset = device_aslr
        .checked_sub(cache.aslr_reference)
        .context("device aslr below host anchor (stale app build?)")?;

    // Classify every named symbol in the original binary: address + is_code.
    let orig_bytes = std::fs::read(&cache.lib).with_context(|| format!("read {}", cache.lib.display()))?;
    let orig = object::File::parse(orig_bytes.as_slice()).context("parse original for stub")?;
    let mut orig_syms: std::collections::HashMap<String, (u64, bool)> = Default::default();
    for sym in orig.symbols() {
        let Ok(name) = sym.name() else { continue };
        if name.is_empty() || sym.is_undefined() {
            continue;
        }
        let is_text = sym.kind() == object::SymbolKind::Text;
        orig_syms.insert(name.to_string(), (sym.address(), is_text));
    }

    let needed = compute_needed_symbols(patch_obj).context("compute needed symbols")?;
    let mut obj = Object::new(BinaryFormat::Elf, Architecture::Aarch64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);

    let mut missing: Vec<&String> = Vec::new();
    for name in &needed {
        let Some((addr, is_text)) = orig_syms.get(name) else {
            missing.push(name);
            continue;
        };
        let abs_addr = addr + aslr_offset;
        if *is_text {
            let code = arm64_jump_stub(abs_addr);
            let off = obj.append_section_data(text, &code, 4);
            obj.add_symbol(Symbol {
                name: name.as_bytes().to_vec(),
                value: off,
                size: code.len() as u64,
                scope: SymbolScope::Linkage,
                kind: ObjKind::Text,
                weak: true,
                section: SymbolSection::Section(text),
                flags: SymbolFlags::None,
            });
        } else {
            obj.add_symbol(Symbol {
                name: name.as_bytes().to_vec(),
                value: abs_addr,
                size: 0,
                scope: SymbolScope::Linkage,
                kind: ObjKind::Data,
                weak: true,
                section: SymbolSection::Absolute,
                flags: SymbolFlags::None,
            });
        }
    }
    if !missing.is_empty() {
        tracing::warn!(missing = ?missing, "stub: symbols absent from original binary");
    }
    obj.write().map_err(|e| anyhow::anyhow!("stub object write: {e}"))
}

/// ARM64: load 64-bit absolute address into X16, then branch (stolen from
/// whisker's identical implementation — it matches the Dioxus one).
fn arm64_jump_stub(addr: u64) -> Vec<u8> {
    let mut code = Vec::with_capacity(20);
    for hw in 0..4u32 {
        let imm = ((addr >> (16 * hw)) & 0xFFFF) as u32;
        // MOVZ X16,#imm,LSL#0 then MOVK X16,#imm,LSL#16/32/48
        let base = if hw == 0 { 0xD280_0010u32 } else { 0xF280_0010u32 | (hw << 21) };
        code.extend_from_slice(&(base | (imm << 5)).to_le_bytes());
    }
    code.extend_from_slice(&0xD61F_0200u32.to_le_bytes()); // BR X16
    code
}

