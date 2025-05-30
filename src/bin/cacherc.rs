#![feature(rustc_private)]

pub extern crate indexmap;
pub extern crate polonius_engine;
pub extern crate rustc_borrowck;
pub extern crate rustc_data_structures;
pub extern crate rustc_driver;
pub extern crate rustc_errors;
pub extern crate rustc_hash;
pub extern crate rustc_hir;
pub extern crate rustc_hir_pretty;
pub extern crate rustc_hir_typeck;
pub extern crate rustc_index;
pub extern crate rustc_interface;
pub extern crate rustc_middle;
pub extern crate rustc_session;
pub extern crate rustc_span;
pub extern crate smallvec;

use cacher::cache::*;
use rustc_driver::{Callbacks, Compilation, run_compiler};
use rustc_hir::def_id::LocalDefId;
use rustc_interface::interface;
use rustc_middle::{
    mir::{
        BorrowCheckResult,
        pretty::{PrettyPrintMirOptions, write_mir_fn},
    },
    query::queries,
    ty::TyCtxt,
    util::Providers,
};
use rustc_session::{EarlyDiagCtxt, config};
use std::env;
use std::sync::atomic::AtomicBool;

static ATOMIC_TRUE: AtomicBool = AtomicBool::new(true);

pub struct RustcCallback;
impl Callbacks for RustcCallback {}

#[inline]
fn override_queries(_session: &rustc_session::Session, local: &mut Providers) {
    local.mir_borrowck = mir_borrowck;
}

#[inline]
fn default_mir_borrowck(
    tcx: TyCtxt<'_>,
    def_id: LocalDefId,
) -> queries::mir_borrowck::ProvidedValue<'_> {
    let mut providers = Providers::default();
    rustc_borrowck::provide(&mut providers);
    (providers.mir_borrowck)(tcx, def_id)
}
#[inline]
fn mir_borrowck(tcx: TyCtxt<'_>, def_id: LocalDefId) -> queries::mir_borrowck::ProvidedValue<'_> {
    // skip const context
    if tcx.hir_body_const_context(def_id.to_def_id()).is_some() {
        return default_mir_borrowck(tcx, def_id);
    }

    let empty_result = BorrowCheckResult {
        concrete_opaque_types: indexmap::IndexMap::default(),
        closure_requirements: None,
        used_mut_upvars: smallvec::SmallVec::new(),
        tainted_by_errors: None,
    };

    let mut compiling_mir = Vec::with_capacity(1_000_000);
    let mut compiling_mir_string = String::new();
    let body = tcx.mir_built(def_id);
    if !body.is_stolen() {
        write_mir_fn(
            tcx,
            &body.borrow(),
            &mut |_, _| Ok(()),
            &mut compiling_mir,
            PrettyPrintMirOptions {
                include_extra_comments: false,
            },
        )
        .unwrap();

        if !compiling_mir.is_empty() {
            compiling_mir_string = unsafe { String::from_utf8_unchecked(compiling_mir) };
            if is_cached(&compiling_mir_string) {
                log::info!("{def_id:?} cache hit");
                return tcx.arena.alloc(empty_result);
            }
        }
    }

    log::info!("{def_id:?} no cache; start mir_borrowck");

    let result = default_mir_borrowck(tcx, def_id);
    let can_cache = result.concrete_opaque_types.is_empty()
        && result.closure_requirements.is_none()
        && result.used_mut_upvars.is_empty()
        && result.tainted_by_errors.is_none()
        && !compiling_mir_string.is_empty();
    if can_cache {
        add_cache(compiling_mir_string);
        log::info!("{def_id:?} cache saved");
    } else {
        log::info!("{def_id:?} cannot be cached due to its mir_borrowck result")
    }
    result
}

pub struct CacherCallback {
    join: Vec<tokio::task::JoinHandle<()>>,
}
impl CacherCallback {
    pub fn new() -> Self {
        Self { join: Vec::new() }
    }
    pub fn join_all(&mut self) {
        let mut new_join = Vec::new();
        std::mem::swap(&mut self.join, &mut new_join);
        for join in new_join {
            let _ = HANDLE.block_on(join);
        }
    }
}
impl Callbacks for CacherCallback {
    fn config(&mut self, config: &mut interface::Config) {
        config.using_internal_features = &ATOMIC_TRUE;
        config.override_queries = Some(override_queries);

        self.join.push(setup_cache());
    }
    fn after_expansion<'tcx>(
        &mut self,
        _compiler: &interface::Compiler,
        _tcx: TyCtxt<'tcx>,
    ) -> Compilation {
        self.join_all();
        Compilation::Continue
    }
    fn after_analysis(&mut self, _compiler: &interface::Compiler, _tcx: TyCtxt<'_>) -> Compilation {
        self.join.push(save_cache());
        Compilation::Continue
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compiler {
    Normal,
    Fast,
}

pub fn run_cacher(compiler: Compiler) -> i32 {
    let ctxt = EarlyDiagCtxt::new(config::ErrorOutputType::default());
    let args = rustc_driver::args::raw_args(&ctxt);

    if compiler == Compiler::Normal {
        return rustc_driver::catch_with_exit_code(|| run_compiler(&args, &mut RustcCallback));
    }
    for arg in &args {
        if arg == "-vV" || arg == "--version" || arg.starts_with("--print") {
            return rustc_driver::catch_with_exit_code(|| run_compiler(&args, &mut RustcCallback));
        }
    }

    let mut callback = CacherCallback::new();
    rustc_driver::catch_with_exit_code(|| {
        run_compiler(&args, &mut callback);
        callback.join_all();
    })
}

fn main() {
    // jemalloc
    // cited from rustc
    #[cfg(all(feature = "jemalloc", not(target_env = "msvc")))]
    {
        use std::os::raw::{c_int, c_void};

        use tikv_jemalloc_sys as jemalloc_sys;

        #[used]
        static _F1: unsafe extern "C" fn(usize, usize) -> *mut c_void = jemalloc_sys::calloc;
        #[used]
        static _F2: unsafe extern "C" fn(*mut *mut c_void, usize, usize) -> c_int =
            jemalloc_sys::posix_memalign;
        #[used]
        static _F3: unsafe extern "C" fn(usize, usize) -> *mut c_void = jemalloc_sys::aligned_alloc;
        #[used]
        static _F4: unsafe extern "C" fn(usize) -> *mut c_void = jemalloc_sys::malloc;
        #[used]
        static _F5: unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void = jemalloc_sys::realloc;
        #[used]
        static _F6: unsafe extern "C" fn(*mut c_void) = jemalloc_sys::free;

        #[cfg(target_os = "macos")]
        {
            unsafe extern "C" {
                fn _rjem_je_zone_register();
            }

            #[used]
            static _F7: unsafe extern "C" fn() = _rjem_je_zone_register;
        }
    }

    simple_logger::SimpleLogger::new()
        .with_level(env::var("CACHER_LOG").map_or(log::LevelFilter::Warn, |v| {
            v.parse().unwrap_or(log::LevelFilter::Warn)
        }))
        .with_colors(true)
        .init()
        .unwrap();

    let fast_result = std::panic::catch_unwind(|| run_cacher(Compiler::Fast));
    let code = match fast_result {
        Ok(0) => 0,
        _ => {
            log::error!("fallback normal rustc");
            run_cacher(Compiler::Normal)
        }
    };

    std::process::exit(code);
}
