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

/// Where the engine keeps its scratch artifacts.
pub fn scratch_dir(workspace_root: &Path) -> PathBuf {
    // Workspace-aware: the app may live inside a cargo workspace whose target
    // root is the workspace root, not <app>/target.
    let mut d = workspace_root.to_path_buf();
    loop {
        let cargo = d.join("Cargo.toml");
        if cargo.exists() {
            let txt = std::fs::read_to_string(&cargo).unwrap_or_default();
            if txt.contains("[workspace]") {
                return d.join("target/.questhotpatch");
            }
        }
        if let Some(parent) = d.parent() {
            d = parent.to_path_buf();
        } else {
            break;
        }
    }
    workspace_root.join("target/.questhotpatch")
}

pub struct PatchSession {
    pub crate_name: String,
    pub hot_funcs: Vec<String>,
    /// Precomputed once at load: defined, non-zero addresses in the old table.
    /// Reused by the full-map filter every patch (was rebuilt per patch: ~0.3s
    /// over 700k entries).
    pub valid_old: std::collections::HashSet<u64>,
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
    pub fn load(
        workspace_root: &Path,
        crate_name: &str,
        hot_funcs: &[String],
        original_binary: &Path,
        real_linker: &Path,
    ) -> Result<Self> {
        let scratch = scratch_dir(workspace_root);
        let captured_rustc = load_captured_args(&scratch.join("rustc"), Some("aarch64-linux-android"))
            .context("load captured rustc args")?;
        let captured_linker =
            load_captured_linker_args(&scratch.join("linker")).context("load captured linker args")?;
        let cache = HotpatchModuleCache::from_path(original_binary)
            .with_context(|| format!("parse original binary {}", original_binary.display()))?;
        let valid_old = cache
            .symbols
            .by_name
            .values()
            .filter(|s| !s.is_undefined && s.address != 0)
            .map(|s| s.address)
            .collect();
        Ok(Self {
            crate_name: crate_name.to_string(),
            hot_funcs: hot_funcs.to_vec(),
            valid_old,
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
            && self.captured_rustc.contains_key(&self.crate_name)
            && !self.captured_linker.is_empty()
    }

    /// Build one patch: fresh `.o` of the tip crate -> stub-object of host
    /// symbol jumps -> NDK link -> diff -> JumpTable.
    ///
    /// `device_aslr` = the `aslr_reference()` the app reported over the wire.
    pub async fn build_patch(&self, device_aslr: u64) -> Result<(JumpTable, PathBuf)> {
        let obj_t0 = std::time::Instant::now();
        let scratch = scratch_dir(&self.workspace_root);
        let objects = scratch.join("objects");
        let patches = scratch.join("patches");
        std::fs::create_dir_all(&objects)?;
        std::fs::create_dir_all(&patches)?;

        // 1. Rebuild ONLY the tip crate as an object file (thin rebuild).
        let captured_rustc = self
            .captured_rustc
            .get(&self.crate_name)
            .with_context(|| format!("no captured rustc invocation for tip crate {}", self.crate_name))?;
        let mut obj_plan = build_obj_plan(captured_rustc, &objects);
        // Determinism across machines/agents: the tip crate source is always
        // <workspace_root>/src/lib.rs; ignore whatever dir the capture ran from.
        if let Some(i) = obj_plan.args.iter().position(|a| a.ends_with(".rs") && !a.starts_with('-')) {
            obj_plan.args[i] = self.workspace_root.join("src/lib.rs").to_string_lossy().into_owned();
        }
        let object_path = run_obj_plan(&obj_plan, &self.rustc_path, &self.workspace_root).await?;
        tracing::info!(t_ms = obj_t0.elapsed().as_millis(), "patch: obj rebuild");

        // 2. Resolve the symbols the patch references against the live app,
        //    via a synthesized stub object (whisker's approach).
        let stub_bytes = build_stub_full(
            &self.cache,
            &object_path,
            device_aslr,
        )?;
        let stub_path = objects.join("stub.o");
        std::fs::write(&stub_path, &stub_bytes)?;
        tracing::info!(t_ms = obj_t0.elapsed().as_millis(), "patch: stub done");

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
        tracing::info!(t_ms = obj_t0.elapsed().as_millis(), "patch: linked");

        // 4. Diff original vs patch symbol tables and build the JumpTable.
        let new_syms = parse_symbol_table(&patch_path).context("parse patch symbols")?;
        tracing::info!(t_ms = obj_t0.elapsed().as_millis(), "patch: parsed new syms");
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
        // Anchor subsecond's apply_patch() on `main` in BOTH binaries (the
        // runtime anchors on dlsym("main"); any mismatch skews every table/stub).
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
        let mut plan = build_jump_table_lean(
            &self.cache.symbols,
            &new_syms,
            patch_path.clone(),
            aslr_ref_main,
            new_base_main,
        );

        // DX-PARITY FULL MAP: every name-overlap Text symbol maps old -> new.
        // (build_jump_table_lean only emits old addresses that exist in the old
        // table, so this is just an O(1) address bound-check against the
        // precomputed set — NOT a rebuild or a per-entry scan.)
        let start_map = std::time::Instant::now();
        let before = plan.table.map.len();
        plan.table.map = plan
            .table
            .map
            .into_iter()
            .filter(|(k, _)| self.valid_old.contains(k))
            .collect();
        tracing::info!(before = before, after = plan.table.map.len(), map_ms = start_map.elapsed().as_millis(), "DX FULL MAP (no filter)");

        // DIAGNOSTIC: is paint_cube / rotate present in old/new and mapped?
        for pat in ["paint_cube", "rotate"] {
            let old_present = self.cache.symbols.by_name.iter().any(|(n, s)| n.contains(pat) && s.address != 0);
            let new_present = new_syms.by_name.iter().any(|(n, s)| n.contains(pat) && s.address != 0);
            let mapped = self.cache.symbols.by_name.iter()
                .filter(|(n, s)| n.contains(pat) && s.address != 0)
                .any(|(_, s)| plan.table.map.contains_key(&s.address));
            tracing::info!(name = %pat, old_present = old_present, new_present = new_present, mapped_oldaddr_in_table = mapped, "DIAG");
        }
        tracing::info!(added_n = plan.report.added.len(), weak_n = plan.report.weak.len(), "patch diff");

        Ok((plan.table, patch_path))
    }
}


/// Full stub-object builder: trampolines for CODE symbols plus absolute-address
/// definitions for DATA symbols (statics/globals like `log::MAX_LOG_LEVEL_FILTER`).
/// Addresses come from `cache.symbols.by_name` — the SAME table that drives the
/// (proven-correct) JumpTable — with the device ASLR slide applied. We do NOT
/// re-parse the ELF here (a second, lossy pass produced stubs that jumped a few
/// bytes off the real symbol -> SIGTRAP on `brk` landing pads).
fn build_stub_full(cache: &HotpatchModuleCache, patch_obj: &std::path::Path, device_aslr: u64) -> Result<Vec<u8>> {
    use object::write::{Object, StandardSection, Symbol, SymbolSection};
    use object::{Architecture, BinaryFormat, Endianness, SymbolFlags, SymbolKind as ObjKind, SymbolScope};

    // ASLR anchor MUST match subsecond::aslr_reference()/apply_patch, which
    // anchors on `main` — NOT on whisker's `whisker_aslr_anchor` (cache.aslr_reference).
    // Mixing the two put a constant (main_off - anchor_off) into every stub target,
    // landing stubs in the middle of functions (SIGTRAP on `brk` pads, and the
    // material-prep SIGSEGVs that looked like data races). The jump table already
    // used `main`; make the stubs agree.
    let main_off = cache
        .symbols
        .by_name
        .get("main")
        .map(|s| s.address)
        .unwrap_or(cache.aslr_reference);
    let aslr_offset = device_aslr
        .checked_sub(main_off)
        .context("device aslr below host main anchor (stale app build?)")?;

    let needed = compute_needed_symbols(patch_obj).context("compute needed symbols")?;
    let mut obj = Object::new(BinaryFormat::Elf, Architecture::Aarch64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);

    let mut missing: Vec<&String> = Vec::new();
    for name in &needed {
        let lookup = name.trim_start_matches("__imp_");
        let Some(sym) = cache.symbols.by_name.get(lookup) else {
            missing.push(name);
            continue;
        };
        if sym.is_undefined || sym.address == 0 {
            continue;
        }
        let abs_addr = sym.address + aslr_offset;
        if matches!(sym.kind, ObjKind::Text) {
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
            // Data/other: absolute-address definition at the host's RUNTIME
            // address of the static (kept as weak so archives that already
            // define the symbol win the link).
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
        tracing::warn!(missing = ?missing, "stub: symbols absent from cache table");
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

/// Like whisker's `build_jump_table`, but WITHOUT the O(old_table) "removed"
/// report pass (700k+ symbol keys walked + sorted every patch, for a message
/// nobody reads). The mapping itself is identical.
fn build_jump_table_lean(
    old: &whisker_dev_server::hotpatch::SymbolTable,
    new: &whisker_dev_server::hotpatch::SymbolTable,
    new_lib: std::path::PathBuf,
    aslr_reference: u64,
    new_base_address: u64,
) -> whisker_dev_server::hotpatch::PatchPlan {
    use object::SymbolKind;
    let mut map = subsecond_types::AddressMap::default();
    let mut added: Vec<String> = Vec::new();
    let mut weak: Vec<String> = Vec::new();

    for (name, new_sym) in &new.by_name {
        let old_sym = match old.by_name.get(name) {
            Some(s) => s,
            None => {
                added.push(name.clone());
                continue;
            }
        };
        // Only hot-patch code, defined, addressable, sized.
        if !matches!(old_sym.kind, SymbolKind::Text)
            || !matches!(new_sym.kind, SymbolKind::Text)
        {
            continue;
        }
        if old_sym.is_undefined || new_sym.is_undefined || old_sym.address == 0 || new_sym.address == 0 {
            continue;
        }
        if old_sym.is_weak || new_sym.is_weak {
            weak.push(name.clone());
        }
        map.insert(old_sym.address, new_sym.address);
    }
    added.sort();
    weak.sort();

    whisker_dev_server::hotpatch::PatchPlan {
        table: subsecond_types::JumpTable {
            lib: new_lib,
            map,
            aslr_reference,
            new_base_address,
            ifunc_count: 0,
        },
        report: whisker_dev_server::hotpatch::DiffReport {
            added,
            removed: Vec::new(),
            weak,
        },
    }
}

