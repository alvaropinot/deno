// Copyright 2018-2019 the Deno authors. All rights reserved. MIT license.
use crate::compiler::TsCompiler;
use crate::deno_dir;
use crate::deno_dir::SourceFile;
use crate::deno_dir::SourceFileFetcher;
use crate::flags;
use crate::global_timer::GlobalTimer;
use crate::import_map::ImportMap;
use crate::ops;
use crate::permissions::DenoPermissions;
use crate::progress::Progress;
use crate::resources;
use crate::resources::ResourceId;
use crate::worker::Worker;
use deno::Buf;
use deno::CoreOp;
use deno::ErrBox;
use deno::Loader;
use deno::ModuleSpecifier;
use deno::PinnedBuf;
use futures::future::Shared;
use futures::Future;
use rand::rngs::StdRng;
use rand::SeedableRng;
use std;
use std::collections::HashMap;
use std::env;
use std::ops::Deref;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;
use tokio::sync::mpsc as async_mpsc;

pub type WorkerSender = async_mpsc::Sender<Buf>;
pub type WorkerReceiver = async_mpsc::Receiver<Buf>;
pub type WorkerChannels = (WorkerSender, WorkerReceiver);
pub type UserWorkerTable = HashMap<ResourceId, Shared<Worker>>;

#[derive(Default)]
pub struct Metrics {
  pub ops_dispatched: AtomicUsize,
  pub ops_completed: AtomicUsize,
  pub bytes_sent_control: AtomicUsize,
  pub bytes_sent_data: AtomicUsize,
  pub bytes_received: AtomicUsize,
  pub resolve_count: AtomicUsize,
  pub compiler_starts: AtomicUsize,
}

/// Isolate cannot be passed between threads but ThreadSafeState can.
/// ThreadSafeState satisfies Send and Sync. So any state that needs to be
/// accessed outside the main V8 thread should be inside ThreadSafeState.
pub struct ThreadSafeState(Arc<State>);

#[cfg_attr(feature = "cargo-clippy", allow(stutter))]
pub struct State {
  pub modules: Arc<Mutex<deno::Modules>>,
  pub main_module: Option<ModuleSpecifier>,
  pub dir: deno_dir::DenoDir,
  pub argv: Vec<String>,
  pub permissions: DenoPermissions,
  pub flags: flags::DenoFlags,
  /// When flags contains a `.import_map_path` option, the content of the
  /// import map file will be resolved and set.
  pub import_map: Option<ImportMap>,
  pub metrics: Metrics,
  pub worker_channels: Mutex<WorkerChannels>,
  pub global_timer: Mutex<GlobalTimer>,
  pub workers: Mutex<UserWorkerTable>,
  pub start_time: Instant,
  /// A reference to this worker's resource.
  pub resource: resources::Resource,
  pub dispatch_selector: ops::OpSelector,
  /// Reference to global progress bar.
  pub progress: Progress,
  pub seeded_rng: Option<Mutex<StdRng>>,

  pub ts_compiler: TsCompiler,
}

impl Clone for ThreadSafeState {
  fn clone(&self) -> Self {
    ThreadSafeState(self.0.clone())
  }
}

impl Deref for ThreadSafeState {
  type Target = Arc<State>;
  fn deref(&self) -> &Self::Target {
    &self.0
  }
}

impl ThreadSafeState {
  pub fn dispatch(
    &self,
    control: &[u8],
    zero_copy: Option<PinnedBuf>,
  ) -> CoreOp {
    ops::dispatch_all(self, control, zero_copy, self.dispatch_selector)
  }
}

pub fn fetch_source_file_and_maybe_compile_async(
  state: &ThreadSafeState,
  module_specifier: &ModuleSpecifier,
) -> impl Future<Item = SourceFile, Error = ErrBox> {
  let state_ = state.clone();

  state_
    .dir
    .fetch_source_file_async(&module_specifier)
    .and_then(move |out| {
      state_
        .clone()
        .ts_compiler
        .compile_async(state_.clone(), &out)
        .map_err(|e| {
          debug!("compiler error exiting!");
          eprintln!("\n{}", e.to_string());
          std::process::exit(1);
        })
    })
}

impl Loader for ThreadSafeState {
  fn resolve(
    &self,
    specifier: &str,
    referrer: &str,
    is_root: bool,
  ) -> Result<ModuleSpecifier, ErrBox> {
    if !is_root {
      if let Some(import_map) = &self.import_map {
        let result = import_map.resolve(specifier, referrer)?;
        if result.is_some() {
          return Ok(result.unwrap());
        }
      }
    }

    ModuleSpecifier::resolve_import(specifier, referrer).map_err(ErrBox::from)
  }

  /// Given an absolute url, load its source code.
  fn load(
    &self,
    module_specifier: &ModuleSpecifier,
  ) -> Box<deno::SourceCodeInfoFuture> {
    self.metrics.resolve_count.fetch_add(1, Ordering::SeqCst);
    Box::new(
      fetch_source_file_and_maybe_compile_async(self, module_specifier).map(
        |source_file| deno::SourceCodeInfo {
          // Real module name, might be different from initial specifier
          // due to redirections.
          code: source_file.js_source(),
          module_name: source_file.url.to_string(),
        },
      ),
    )
  }
}

impl ThreadSafeState {
  pub fn new(
    flags: flags::DenoFlags,
    argv_rest: Vec<String>,
    dispatch_selector: ops::OpSelector,
    progress: Progress,
  ) -> Self {
    let custom_root = env::var("DENO_DIR").map(String::into).ok();

    let (worker_in_tx, worker_in_rx) = async_mpsc::channel::<Buf>(1);
    let (worker_out_tx, worker_out_rx) = async_mpsc::channel::<Buf>(1);
    let internal_channels = (worker_out_tx, worker_in_rx);
    let external_channels = (worker_in_tx, worker_out_rx);
    let resource = resources::add_worker(external_channels);

    let dir = deno_dir::DenoDir::new(
      custom_root,
      progress.clone(),
      !flags.reload,
      flags.no_fetch,
    ).unwrap();

    let main_module: Option<ModuleSpecifier> = if argv_rest.len() <= 1 {
      None
    } else {
      let root_specifier = argv_rest[1].clone();
      match ModuleSpecifier::resolve_url_or_path(&root_specifier) {
        Ok(specifier) => Some(specifier),
        Err(e) => {
          // TODO: handle unresolvable specifier
          panic!("Unable to resolve root specifier: {:?}", e);
        }
      }
    };

    let mut import_map = None;
    if let Some(file_name) = &flags.import_map_path {
      let base_url = match &main_module {
        Some(module_specifier) => module_specifier.clone(),
        None => unreachable!(),
      };

      match ImportMap::load(&base_url.to_string(), file_name) {
        Ok(map) => import_map = Some(map),
        Err(err) => {
          println!("{:?}", err);
          panic!("Error parsing import map");
        }
      }
    }

    let mut seeded_rng = None;
    if let Some(seed) = flags.seed {
      seeded_rng = Some(Mutex::new(StdRng::seed_from_u64(seed)));
    };

    let modules = Arc::new(Mutex::new(deno::Modules::new()));

    let ts_compiler =
      TsCompiler::new(dir.clone(), !flags.reload, flags.config_path.clone());

    ThreadSafeState(Arc::new(State {
      main_module,
      modules,
      dir,
      argv: argv_rest,
      permissions: DenoPermissions::from_flags(&flags),
      flags,
      import_map,
      metrics: Metrics::default(),
      worker_channels: Mutex::new(internal_channels),
      global_timer: Mutex::new(GlobalTimer::new()),
      workers: Mutex::new(UserWorkerTable::new()),
      start_time: Instant::now(),
      resource,
      dispatch_selector,
      progress,
      seeded_rng,
      ts_compiler,
    }))
  }

  /// Read main module from argv
  pub fn main_module(&self) -> Option<ModuleSpecifier> {
    match &self.main_module {
      Some(module_specifier) => Some(module_specifier.clone()),
      None => None,
    }
  }

  #[inline]
  pub fn check_read(&self, filename: &str) -> Result<(), ErrBox> {
    self.permissions.check_read(filename)
  }

  #[inline]
  pub fn check_write(&self, filename: &str) -> Result<(), ErrBox> {
    self.permissions.check_write(filename)
  }

  #[inline]
  pub fn check_env(&self) -> Result<(), ErrBox> {
    self.permissions.check_env()
  }

  #[inline]
  pub fn check_net(&self, host_and_port: &str) -> Result<(), ErrBox> {
    self.permissions.check_net(host_and_port)
  }

  #[inline]
  pub fn check_net_url(&self, url: url::Url) -> Result<(), ErrBox> {
    self.permissions.check_net_url(url)
  }

  #[inline]
  pub fn check_run(&self) -> Result<(), ErrBox> {
    self.permissions.check_run()
  }

  #[cfg(test)]
  pub fn mock(argv: Vec<String>) -> ThreadSafeState {
    ThreadSafeState::new(
      flags::DenoFlags::default(),
      argv,
      ops::op_selector_std,
      Progress::new(),
    )
  }

  pub fn metrics_op_dispatched(
    &self,
    bytes_sent_control: usize,
    bytes_sent_data: usize,
  ) {
    self.metrics.ops_dispatched.fetch_add(1, Ordering::SeqCst);
    self
      .metrics
      .bytes_sent_control
      .fetch_add(bytes_sent_control, Ordering::SeqCst);
    self
      .metrics
      .bytes_sent_data
      .fetch_add(bytes_sent_data, Ordering::SeqCst);
  }

  pub fn metrics_op_completed(&self, bytes_received: usize) {
    self.metrics.ops_completed.fetch_add(1, Ordering::SeqCst);
    self
      .metrics
      .bytes_received
      .fetch_add(bytes_received, Ordering::SeqCst);
  }
}

#[test]
fn thread_safe() {
  fn f<S: Send + Sync>(_: S) {}
  f(ThreadSafeState::mock(vec![
    String::from("./deno"),
    String::from("hello.js"),
  ]));
}
