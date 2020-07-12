// Copyright 2018-2020 the Deno authors. All rights reserved. MIT license.
use super::dispatch_json::{Deserialize, JsonOp, Value};
use crate::fmt_errors::JSError;
use crate::global_state::GlobalState;
use crate::import_map::ImportMap;
use crate::op_error::OpError;
use crate::ops::io::get_stdio;
use crate::permissions::Permissions;
use crate::startup_data;
use crate::state::State;
use crate::tokio_util::create_basic_runtime;
use crate::web_worker::WebWorker;
use crate::web_worker::WebWorkerHandle;
use crate::worker::WorkerEvent;
use deno_core::CoreIsolate;
use deno_core::ErrBox;
use deno_core::ModuleSpecifier;
use deno_core::ZeroCopyBuf;
use futures::future::FutureExt;
use std::convert::From;
use std::path::Path;
use std::thread::JoinHandle;

pub fn init(i: &mut CoreIsolate, s: &State) {
  i.register_op("op_create_worker", s.stateful_json_op(op_create_worker));
  i.register_op(
    "op_host_terminate_worker",
    s.stateful_json_op(op_host_terminate_worker),
  );
  i.register_op(
    "op_host_post_message",
    s.stateful_json_op(op_host_post_message),
  );
  i.register_op(
    "op_host_get_message",
    s.stateful_json_op(op_host_get_message),
  );
}

fn create_web_worker(
  worker_id: u32,
  name: String,
  global_state: GlobalState,
  permissions: Permissions,
  specifier: ModuleSpecifier,
  has_deno_namespace: bool,
  import_map_path: Option<String>,
) -> Result<WebWorker, ErrBox> {
  let maybe_import_map = match import_map_path.as_ref() {
    None => None,
    Some(file_path) => {
      permissions.check_read(Path::new(file_path))?;
      Some(ImportMap::load(file_path)?)
    }
  };

  let state = State::new_for_worker(
    global_state,
    Some(permissions),
    specifier,
    maybe_import_map,
  )?;

  let mut worker = WebWorker::new(
    name.clone(),
    startup_data::deno_isolate_init(),
    state,
    has_deno_namespace,
  );

  if has_deno_namespace {
    let state_rc = CoreIsolate::state(&worker.isolate);
    let state = state_rc.borrow();
    let mut resource_table = state.resource_table.borrow_mut();
    let (stdin, stdout, stderr) = get_stdio();
    if let Some(stream) = stdin {
      resource_table.add("stdin", Box::new(stream));
    }
    if let Some(stream) = stdout {
      resource_table.add("stdout", Box::new(stream));
    }
    if let Some(stream) = stderr {
      resource_table.add("stderr", Box::new(stream));
    }
  }

  // Instead of using name for log we use `worker-${id}` because
  // WebWorkers can have empty string as name.
  let script = format!(
    "bootstrap.workerRuntime(\"{}\", {}, \"worker-{}\")",
    name, worker.has_deno_namespace, worker_id
  );
  worker.execute(&script)?;

  Ok(worker)
}

pub struct RunWorkerThreadArgs {
  worker_id: u32,
  name: String,
  global_state: GlobalState,
  permissions: Permissions,
  specifier: ModuleSpecifier,
  has_deno_namespace: bool,
  maybe_source_code: Option<String>,
  import_map: Option<String>,
}
// TODO(bartlomieju): check if order of actions is aligned to Worker spec
fn run_worker_thread(
  args: RunWorkerThreadArgs,
) -> Result<(JoinHandle<()>, WebWorkerHandle), ErrBox> {
  let (handle_sender, handle_receiver) =
    std::sync::mpsc::sync_channel::<Result<WebWorkerHandle, ErrBox>>(1);

  let builder =
    std::thread::Builder::new().name(format!("deno-worker-{}", args.worker_id));
  let join_handle = builder.spawn(move || {
    // Any error inside this block is terminal:
    // - JS worker is useless - meaning it throws an exception and can't do anything else,
    //  all action done upon it should be noops
    // - newly spawned thread exits
    let result = create_web_worker(
      args.worker_id,
      args.name,
      args.global_state,
      args.permissions,
      args.specifier.clone(),
      args.has_deno_namespace,
      args.import_map,
    );

    if let Err(err) = result {
      handle_sender.send(Err(err)).unwrap();
      return;
    }

    let mut worker = result.unwrap();
    let name = worker.name.to_string();
    // Send thread safe handle to newly created worker to host thread
    handle_sender.send(Ok(worker.thread_safe_handle())).unwrap();
    drop(handle_sender);

    // At this point the only method of communication with host
    // is using `worker.internal_channels`.
    //
    // Host can already push messages and interact with worker.
    //
    // Next steps:
    // - create tokio runtime
    // - load provided module or code
    // - start driving worker's event loop

    let mut rt = create_basic_runtime();

    // TODO: run with using select with terminate

    // Execute provided source code immediately
    let result = if let Some(source_code) = args.maybe_source_code {
      worker.execute(&source_code)
    } else {
      // TODO(bartlomieju): add "type": "classic", ie. ability to load
      // script instead of module
      let load_future = worker.execute_module(&args.specifier).boxed_local();

      rt.block_on(load_future)
    };

    if let Err(e) = result {
      let mut sender = worker.internal_channels.sender.clone();
      sender
        .try_send(WorkerEvent::TerminalError(e))
        .expect("Failed to post message to host");

      // Failure to execute script is a terminal error, bye, bye.
      return;
    }

    // TODO(bartlomieju): this thread should return result of event loop
    // that means that we should store JoinHandle to thread to ensure
    // that it actually terminates.
    rt.block_on(worker).expect("Panic in event loop");
    debug!("Worker thread shuts down {}", &name);
  })?;

  let worker_handle = handle_receiver.recv().unwrap()?;
  Ok((join_handle, worker_handle))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateWorkerArgs {
  name: Option<String>,
  specifier: String,
  has_source_code: bool,
  source_code: String,
  use_deno_namespace: bool,
  import_map: Option<String>,
}

/// Create worker as the host
fn op_create_worker(
  state: &State,
  args: Value,
  _data: &mut [ZeroCopyBuf],
) -> Result<JsonOp, OpError> {
  let args: CreateWorkerArgs = serde_json::from_value(args)?;

  let specifier = args.specifier.clone();
  let maybe_source_code = if args.has_source_code {
    Some(args.source_code.clone())
  } else {
    None
  };
  let args_name = args.name;
  let use_deno_namespace = args.use_deno_namespace;
  if use_deno_namespace {
    state.check_unstable("Worker.deno");
  }
  let import_map = args.import_map;
  let parent_state = state.clone();
  let mut state = state.borrow_mut();
  let global_state = state.global_state.clone();
  let permissions = state.permissions.clone();
  let worker_id = state.next_worker_id;
  state.next_worker_id += 1;
  drop(state);

  let module_specifier = ModuleSpecifier::resolve_url(&specifier)?;
  let worker_name = args_name.unwrap_or_else(|| "".to_string());

  let (join_handle, worker_handle) = run_worker_thread(RunWorkerThreadArgs {
    worker_id,
    name: worker_name,
    global_state,
    permissions,
    specifier: module_specifier,
    has_deno_namespace: use_deno_namespace,
    maybe_source_code,
    import_map,
  })
  .map_err(|e| OpError::other(e.to_string()))?;
  // At this point all interactions with worker happen using thread
  // safe handler returned from previous function call
  let mut parent_state = parent_state.borrow_mut();
  parent_state
    .workers
    .insert(worker_id, (join_handle, worker_handle));

  Ok(JsonOp::Sync(json!({ "id": worker_id })))
}

#[derive(Deserialize)]
struct WorkerArgs {
  id: i32,
}

fn op_host_terminate_worker(
  state: &State,
  args: Value,
  _data: &mut [ZeroCopyBuf],
) -> Result<JsonOp, OpError> {
  let args: WorkerArgs = serde_json::from_value(args)?;
  let id = args.id as u32;
  let mut state = state.borrow_mut();
  let (join_handle, worker_handle) =
    state.workers.remove(&id).expect("No worker handle found");
  worker_handle.terminate();
  join_handle.join().expect("Panic in worker thread");
  Ok(JsonOp::Sync(json!({})))
}

fn serialize_worker_event(event: WorkerEvent) -> Value {
  match event {
    WorkerEvent::Message(buf) => json!({ "type": "msg", "data": buf }),
    WorkerEvent::TerminalError(error) => {
      let mut serialized_error = json!({
        "type": "terminalError",
        "error": {
          "message": error.to_string(),
        }
      });

      if let Ok(js_error) = error.downcast::<JSError>() {
        serialized_error = json!({
          "type": "terminalError",
          "error": {
            "message": js_error.message,
            "fileName": js_error.script_resource_name,
            "lineNumber": js_error.line_number,
            "columnNumber": js_error.start_column,
          }
        });
      }

      serialized_error
    }
    WorkerEvent::Error(error) => {
      let mut serialized_error = json!({
        "type": "error",
        "error": {
          "message": error.to_string(),
        }
      });

      if let Ok(js_error) = error.downcast::<JSError>() {
        serialized_error = json!({
          "type": "error",
          "error": {
            "message": js_error.message,
            "fileName": js_error.script_resource_name,
            "lineNumber": js_error.line_number,
            "columnNumber": js_error.start_column,
          }
        });
      }

      serialized_error
    }
  }
}

/// Get message from guest worker as host
fn op_host_get_message(
  state: &State,
  args: Value,
  _data: &mut [ZeroCopyBuf],
) -> Result<JsonOp, OpError> {
  let args: WorkerArgs = serde_json::from_value(args)?;
  let id = args.id as u32;
  let worker_handle = {
    let state_ = state.borrow();
    let (_join_handle, worker_handle) =
      state_.workers.get(&id).expect("No worker handle found");
    worker_handle.clone()
  };
  let state_ = state.clone();
  let op = async move {
    let response = match worker_handle.get_event().await? {
      Some(event) => {
        // Terminal error means that worker should be removed from worker table.
        if let WorkerEvent::TerminalError(_) = &event {
          let mut state_ = state_.borrow_mut();
          if let Some((join_handle, mut worker_handle)) =
            state_.workers.remove(&id)
          {
            worker_handle.sender.close_channel();
            join_handle.join().expect("Worker thread panicked");
          }
        }
        serialize_worker_event(event)
      }
      None => {
        // Worker shuts down
        let mut state_ = state_.borrow_mut();
        // Try to remove worker from workers table - NOTE: `Worker.terminate()` might have been called
        // already meaning that we won't find worker in table - in that case ignore.
        if let Some((join_handle, mut worker_handle)) =
          state_.workers.remove(&id)
        {
          worker_handle.sender.close_channel();
          join_handle.join().expect("Worker thread panicked");
        }
        json!({ "type": "close" })
      }
    };
    Ok(response)
  };
  Ok(JsonOp::Async(op.boxed_local()))
}

/// Post message to guest worker as host
fn op_host_post_message(
  state: &State,
  args: Value,
  data: &mut [ZeroCopyBuf],
) -> Result<JsonOp, OpError> {
  assert_eq!(data.len(), 1, "Invalid number of arguments");
  let args: WorkerArgs = serde_json::from_value(args)?;
  let id = args.id as u32;
  let msg = Vec::from(&*data[0]).into_boxed_slice();

  debug!("post message to worker {}", id);
  let state = state.borrow();
  let (_, worker_handle) =
    state.workers.get(&id).expect("No worker handle found");
  worker_handle
    .post_message(msg)
    .map_err(|e| OpError::other(e.to_string()))?;
  Ok(JsonOp::Sync(json!({})))
}
