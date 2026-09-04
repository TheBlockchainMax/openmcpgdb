use crate::{
    config::ServerConfig,
    error::{OpenMcpGdbError, Result},
    gdb::GdbBackend,
    protocol::{CurrentCodePayload, DebuggerResponse, DebuggerState},
};
use std::{collections::BTreeMap, path::PathBuf, process::Stdio, thread};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Duration, sleep};

#[derive(Debug)]
pub enum ToolOperation {
    Execute { executable_path: String },
    Run,
    GdbServer { ip: String, port: u16, pid: i64 },
    TargetRemote { ip: String, port: u16 },
    SetThread { id: i64 },
    SetFrame { id: i64 },
    AddBreakpoint { filename: String, linenumber: u64 },
    ClearBreakpoint { filename: String, linenumber: u64 },
    EnableBreakpoint { filename: String, linenumber: u64 },
    DisableBreakpoint { filename: String, linenumber: u64 },
    ListBreakpoint,
    Next,
    Step,
    Continue,
    Interrupt,
    AddVariable { var: String },
    DelVariable { var: String },
    DebuggerState,
    VariableList,
    CurrentCode,
    FullBacktrace,
    InfoThreads,
    Print { var: String, value: Option<String> },
    SetVar { var: String, value: String },
    InfoRegs,
    Quit,
    Kill,
    ResetBackToNotAttached,
    SetDisplayLinesBeforeCurrent { size: usize },
    SetDisplayLinesAfterCurrent { size: usize },
    SetDisplayBacktrace { size: usize },
    SetDisplayVariableList { size: usize },
    Custom { cmd: String },
}

enum WorkerMessage {
    Execute {
        operation: ToolOperation,
        response_tx: oneshot::Sender<Result<DebuggerResponse>>,
    },
}

#[derive(Clone)]
pub struct SessionWorkerHandle {
    request_tx: mpsc::Sender<WorkerMessage>,
}

impl SessionWorkerHandle {
    pub async fn execute(&self, operation: ToolOperation) -> Result<DebuggerResponse> {
        let (response_tx, response_rx) = oneshot::channel();
        self.request_tx
            .send(WorkerMessage::Execute {
                operation,
                response_tx,
            })
            .await
            .map_err(|_| OpenMcpGdbError::SessionClosed)?;
        response_rx
            .await
            .map_err(|_| OpenMcpGdbError::SessionClosed)?
    }
}

pub fn spawn_session_thread(
    config: ServerConfig,
    mut backend: Box<dyn GdbBackend>,
) -> SessionWorkerHandle {
    let (request_tx, mut request_rx) = mpsc::channel::<WorkerMessage>(64);

    // Every client gets a dedicated worker thread and runtime for isolation.
    thread::spawn(move || {
        let runtime_result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();

        let Ok(runtime) = runtime_result else {
            return;
        };

        runtime.block_on(async move {
            let mut core = SessionCore::new(config, &mut backend);
            while let Some(message) = request_rx.recv().await {
                match message {
                    WorkerMessage::Execute {
                        operation,
                        response_tx,
                    } => {
                        let result = core.execute(operation).await;
                        let result = core.enrich_crash_response_result(result).await;
                        let _ = response_tx.send(result);
                    }
                }
            }
            let _ = core.shutdown().await;
            let _ = backend.stop().await;
        });
    });

    SessionWorkerHandle { request_tx }
}

struct SessionCore<'a> {
    config: ServerConfig,
    backend: &'a mut Box<dyn GdbBackend>,
    debugger_state: DebuggerState,
    watched_variables: Vec<String>,
    executable_path: Option<PathBuf>,
    gdbserver_child: Option<tokio::process::Child>,
    last_error: String,
}

impl<'a> SessionCore<'a> {
    async fn enrich_crash_response_result(
        &mut self,
        result: Result<DebuggerResponse>,
    ) -> Result<DebuggerResponse> {
        match result {
            Ok(response) => self.enrich_crash_response(response).await,
            Err(err) => Err(err),
        }
    }

    async fn enrich_crash_response(
        &mut self,
        mut response: DebuggerResponse,
    ) -> Result<DebuggerResponse> {
        if !matches!(
            response.debugger_state,
            DebuggerState::SigSegv | DebuggerState::SigFpe | DebuggerState::SigIll
        ) {
            return Ok(response);
        }

        if response.backtrace.is_some() {
            return Ok(response);
        }

        if self.executable_path.is_none() {
            return Ok(response);
        }

        let (backtrace, current_func) = self.collect_backtrace(true).await?;
        response.backtrace = Some(backtrace);
        if response.current_func.is_none() {
            response.current_func = current_func;
        }
        Ok(response)
    }

    fn recover_error_state_without_restart(&mut self) {
        if self.debugger_state == DebuggerState::Error {
            self.debugger_state = self.recoverable_base_state();
        }
        self.last_error.clear();
    }

    fn recoverable_base_state(&self) -> DebuggerState {
        if self.executable_path.is_some() {
            DebuggerState::Attached
        } else {
            DebuggerState::NotAttached
        }
    }

    fn new(config: ServerConfig, backend: &'a mut Box<dyn GdbBackend>) -> Self {
        Self {
            config,
            backend,
            debugger_state: DebuggerState::NotAttached,
            watched_variables: Vec::new(),
            executable_path: None,
            gdbserver_child: None,
            last_error: String::new(),
        }
    }

    async fn execute(&mut self, operation: ToolOperation) -> Result<DebuggerResponse> {
        match operation {
            ToolOperation::Execute { executable_path } => {
                self.execute_attach(executable_path).await
            }
            ToolOperation::Run => {
                self.execute_run().await
            }
            ToolOperation::GdbServer { ip, port, pid } => self.execute_gdbserver(ip, port, pid).await,
            ToolOperation::TargetRemote { ip, port } => {
                self.execute_target_remote(ip, port).await
            }
            ToolOperation::SetThread { id } => {
                self.execute_recoverable_command_with_output(&format!("thread {id}"), None)
                    .await
            }
            ToolOperation::SetFrame { id } => {
                self.execute_recoverable_command_with_output(&format!("frame {id}"), None)
                    .await
            }
            ToolOperation::AddBreakpoint {
                filename,
                linenumber,
            } => {
                self.execute_recoverable_command_with_output(
                    &format!("break {filename}:{linenumber}"),
                    None,
                )
                .await
            }
            ToolOperation::ClearBreakpoint {
                filename,
                linenumber,
            } => {
                self.execute_command(&format!("clear {filename}:{linenumber}"), None)
                    .await
            }
            ToolOperation::EnableBreakpoint {
                filename,
                linenumber,
            } => {
                self.execute_breakpoint_action_by_location(BreakpointAction::Enable, &filename, linenumber)
                    .await
            }
            ToolOperation::DisableBreakpoint {
                filename,
                linenumber,
            } => {
                self.execute_breakpoint_action_by_location(BreakpointAction::Disable, &filename, linenumber)
                    .await
            }
            ToolOperation::ListBreakpoint => self.list_breakpoint_response().await,
            ToolOperation::Next => {
                self.execute_with_full_snapshot("next", DebuggerState::StoppedAtStepping)
                    .await
            }
            ToolOperation::Step => {
                self.execute_with_full_snapshot("step", DebuggerState::StoppedAtStepping)
                    .await
            }
            ToolOperation::Continue => {
                self.execute_with_full_snapshot("continue", DebuggerState::Running)
                    .await
            }
            ToolOperation::Interrupt => self.execute_interrupt().await,
            ToolOperation::AddVariable { var } => {
                if !self
                    .watched_variables
                    .iter()
                    .any(|existing| existing == &var)
                {
                    self.watched_variables.push(var);
                }
                self.variable_list_response().await
            }
            ToolOperation::DelVariable { var } => {
                self.watched_variables.retain(|existing| existing != &var);
                self.variable_list_response().await
            }
            ToolOperation::DebuggerState => Ok(self.base_response()),
            ToolOperation::VariableList => self.variable_list_response().await,
            ToolOperation::CurrentCode => self.current_code_response().await,
            ToolOperation::FullBacktrace => self.full_backtrace_response().await,
            ToolOperation::InfoThreads => {
                self.execute_recoverable_command_with_output("info threads", None)
                    .await
            }
            ToolOperation::Print { var, value } => {
                if let Some(value) = value {
                    self.execute_command(&format!("set variable {var} = {value}"), None)
                        .await
                } else {
                    self.execute_print(&var)
                        .await
                }
            }
            ToolOperation::SetVar { var, value } => {
                self.execute_command(&format!("set variable {var} = {value}"), None)
                    .await
            }
            ToolOperation::InfoRegs => {
                if self.debugger_state == DebuggerState::NotAttached
                    || self.debugger_state == DebuggerState::FailedToAttach
                    || self.debugger_state == DebuggerState::GdbServerFailedToAttach
                    || self.debugger_state == DebuggerState::Exited
                {
                    return Ok(self.base_response());
                }
                self.execute_info_regs().await
            }
            ToolOperation::Quit => {
                if self.executable_path.is_none() {
                    let _ = self.stop_gdbserver_process().await;
                    let _ = self.backend.stop().await;
                    self.debugger_state = DebuggerState::NotAttached;
                    self.last_error.clear();
                    self.watched_variables.clear();
                    return Ok(self.base_response());
                }
                let _ = self.stop_gdbserver_process().await;
                // Interrupt running debuggee before sending quit command.
                if self.debugger_state == DebuggerState::Running {
                    let _ = self.backend.interrupt().await;
                }
                let quit_result = self.backend.exec("quit").await;
                let stop_result = self.backend.stop().await;

                match (quit_result, stop_result) {
                    (Ok(_), Ok(_)) => {
                        self.debugger_state = DebuggerState::NotAttached;
                        self.executable_path = None;
                        self.watched_variables.clear();
                        self.last_error.clear();
                        Ok(self.base_response())
                    }
                    (quit_err, stop_err) => {
                        let mut errors = Vec::new();
                        if let Err(err) = quit_err {
                            errors.push(format!("quit failed: {err}"));
                        }
                        if let Err(err) = stop_err {
                            errors.push(format!("stop failed: {err}"));
                        }
                        self.debugger_state = DebuggerState::Error;
                        self.last_error = errors.join("; ");
                        Ok(self.base_response().with_error(self.last_error.clone()))
                    }
                }
            }
            ToolOperation::Kill => {
                self.execute_kill().await
            }
            ToolOperation::ResetBackToNotAttached => {
                let _ = self.stop_gdbserver_process().await;
                let _ = self.backend.stop().await;
                self.debugger_state = DebuggerState::NotAttached;
                self.executable_path = None;
                self.watched_variables.clear();
                self.last_error.clear();
                Ok(self.base_response())
            }
            ToolOperation::SetDisplayLinesBeforeCurrent { size } => {
                if size == 0 {
                    self.last_error = "display_lines_before_current must be > 0".to_string();
                    self.debugger_state = DebuggerState::Error;
                    return Ok(self.base_response().with_error(self.last_error.clone()));
                }
                self.config.display_lines_before_current = size;
                self.recover_error_state_without_restart();
                Ok(self.base_response())
            }
            ToolOperation::SetDisplayLinesAfterCurrent { size } => {
                if size == 0 {
                    self.last_error = "display_lines_after_current must be > 0".to_string();
                    self.debugger_state = DebuggerState::Error;
                    return Ok(self.base_response().with_error(self.last_error.clone()));
                }
                self.config.display_lines_after_current = size;
                self.recover_error_state_without_restart();
                Ok(self.base_response())
            }
            ToolOperation::SetDisplayBacktrace { size } => {
                if size == 0 {
                    self.last_error = "display_backtrace must be > 0".to_string();
                    self.debugger_state = DebuggerState::Error;
                    return Ok(self.base_response().with_error(self.last_error.clone()));
                }
                self.config.display_backtrace = size;
                self.recover_error_state_without_restart();
                Ok(self.base_response())
            }
            ToolOperation::SetDisplayVariableList { size } => {
                if size == 0 {
                    self.last_error = "display_variable_list must be > 0".to_string();
                    self.debugger_state = DebuggerState::Error;
                    return Ok(self.base_response().with_error(self.last_error.clone()));
                }
                self.config.display_variable_list = size;
                self.recover_error_state_without_restart();
                Ok(self.base_response())
            }
            ToolOperation::Custom { cmd } => self.execute_command_with_output(&cmd, None).await,
        }
    }

    async fn execute_recoverable_command_with_output(
        &mut self,
        command: &str,
        fallback_state: Option<DebuggerState>,
    ) -> Result<DebuggerResponse> {
        let previous_state = self.debugger_state;
        let response = self
            .execute_command_with_output(command, fallback_state)
            .await?;
        if response.debugger_state != DebuggerState::Error {
            return Ok(response);
        }

        let error_text = response.error.to_ascii_lowercase();
        if !is_recoverable_command_error(&error_text) {
            return Ok(response);
        }

        let recovered_state = if previous_state == DebuggerState::Error {
            self.recoverable_base_state()
        } else {
            previous_state
        };
        self.debugger_state = recovered_state;
        self.last_error.clear();

        let mut soft_error_response = self.base_response();
        soft_error_response.error = response.error;
        soft_error_response.command_output = response.command_output;
        Ok(soft_error_response)
    }

    async fn shutdown(&mut self) -> Result<()> {
        self.stop_gdbserver_process().await
    }

    async fn execute_attach(&mut self, executable_path: String) -> Result<DebuggerResponse> {
        let executable = PathBuf::from(executable_path.clone());
        if !executable.is_absolute() {
            self.debugger_state = DebuggerState::FailedToAttach;
            self.last_error = "executable_path must be absolute".to_string();
            return Ok(self.base_response().with_error(self.last_error.clone()));
        }
        match self.backend.start(&executable).await {
            Ok(_) => {
                self.executable_path = Some(executable);
                self.debugger_state = DebuggerState::Attached;
                self.last_error.clear();
                Ok(self.base_response())
            }
            Err(err) => {
                self.debugger_state = DebuggerState::FailedToAttach;
                self.last_error = err.to_string();
                Ok(self.base_response().with_error(self.last_error.clone()))
            }
        }
    }

    async fn execute_command(
        &mut self,
        command: &str,
        fallback_state: Option<DebuggerState>,
    ) -> Result<DebuggerResponse> {
        self.execute_command_internal(command, fallback_state, false)
            .await
    }

    async fn execute_command_with_output(
        &mut self,
        command: &str,
        fallback_state: Option<DebuggerState>,
    ) -> Result<DebuggerResponse> {
        self.execute_command_internal(command, fallback_state, true)
            .await
    }

    async fn execute_command_internal(
        &mut self,
        command: &str,
        fallback_state: Option<DebuggerState>,
        include_output: bool,
    ) -> Result<DebuggerResponse> {
        if self.executable_path.is_none() {
            // Tool commands must not implicitly attach/start gdb. Call execute first.
            return Ok(self.base_response());
        }

        if self.debugger_state == DebuggerState::Running && command != "continue" {
            // Re-sync command stream before issuing interactive queries after continue.
            let _ = self.backend.interrupt().await;
            let _ = self.backend.exec("printf \"\"").await;
            self.debugger_state = DebuggerState::StoppedAtStepping;
        }

        let previous_state = self.debugger_state;
        let result = self.backend.exec(command).await;
        match result {
            Ok(output) => {
                self.update_state_from_output(&output, fallback_state);
                if self.debugger_state == DebuggerState::Error {
                    self.last_error = normalized_command_output(&output)
                        .unwrap_or_else(|| "gdb command failed".to_string());
                } else {
                    self.last_error.clear();
                }
                if self.debugger_state == DebuggerState::Error
                    && previous_state == DebuggerState::Error
                    && !looks_like_gdb_error(&output)
                {
                    // Allow recovery from previous Error state when a command succeeds.
                    self.debugger_state = self.recoverable_base_state();
                    self.last_error.clear();
                }
                // Make successful tool calls recoverable after stale error state.
                if self.debugger_state != DebuggerState::Error
                    && self.debugger_state != DebuggerState::FailedToAttach
                    && self.debugger_state != DebuggerState::GdbServerFailedToAttach
                {
                    self.last_error.clear();
                }
                let mut response = self.base_response();
                if include_output {
                    response.command_output = normalized_command_output(&output);
                }
                Ok(response)
            }
            Err(err) => {
                self.last_error = err.to_string();
                self.debugger_state = DebuggerState::Error;
                let response = self.base_response().with_error(self.last_error.clone());
                Ok(response)
            }
        }
    }

    async fn execute_print(&mut self, var: &str) -> Result<DebuggerResponse> {
        let previous_state = self.debugger_state;
        let response = self
            .execute_command_with_output(&format!("print {var}"), None)
            .await?;

        if response.debugger_state != DebuggerState::Error {
            return Ok(response);
        }

        // Printing a symbol out of scope should be recoverable: keep session state and return
        // the command error text without forcing a global debugger error latch.
        let recovered_state = if previous_state == DebuggerState::Error {
            if self.executable_path.is_some() {
                DebuggerState::Attached
            } else {
                DebuggerState::NotAttached
            }
        } else {
            previous_state
        };

        self.debugger_state = recovered_state;
        self.last_error.clear();

        let mut soft_error_response = self.base_response();
        soft_error_response.command_output = response.command_output;
        soft_error_response.error = response.error;
        Ok(soft_error_response)
    }

    async fn execute_info_regs(&mut self) -> Result<DebuggerResponse> {
        let response = self.execute_command_with_output("info all-registers", None).await?;
        let output = response
            .command_output
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();

        if output.contains("no registers") || output.contains("has no registers") {
            // Keep session attached even when inferior is not currently stopped in a frame.
            self.debugger_state = DebuggerState::Attached;
            self.last_error.clear();
            let mut adjusted = self.base_response();
            adjusted.command_output = response.command_output;
            return Ok(adjusted);
        }

        Ok(response)
    }

    async fn execute_breakpoint_action_by_location(
        &mut self,
        action: BreakpointAction,
        filename: &str,
        linenumber: u64,
    ) -> Result<DebuggerResponse> {
        if self.executable_path.is_none() {
            return Ok(self.base_response());
        }

        if self.debugger_state == DebuggerState::Running {
            let _ = self.backend.interrupt().await;
            let _ = self.backend.exec("printf \"\"").await;
            self.debugger_state = DebuggerState::StoppedAtStepping;
        }

        let ids = self
            .resolve_breakpoint_ids_by_location(filename, linenumber)
            .await?;

        if ids.is_empty() {
            self.debugger_state = DebuggerState::Error;
            self.last_error = format!("No breakpoint at {filename}:{linenumber}.");
            return Ok(self.base_response().with_error(self.last_error.clone()));
        }

        for id in ids {
            let command = action.command_for_id(&id);
            let output = self.backend.exec(&command).await?;
            self.update_state_from_output(&output, None);
            if self.debugger_state == DebuggerState::Error {
                self.last_error = normalized_command_output(&output)
                    .unwrap_or_else(|| "gdb breakpoint operation failed".to_string());
                return Ok(self.base_response().with_error(self.last_error.clone()));
            }
        }

        self.recover_error_state_without_restart();
        Ok(self.base_response())
    }

    async fn resolve_breakpoint_ids_by_location(
        &mut self,
        filename: &str,
        linenumber: u64,
    ) -> Result<Vec<String>> {
        let target = std::path::Path::new(filename);
        let output = self.backend.exec("info breakpoints").await?;
        let mut ids = Vec::new();

        for line in output.lines() {
            if let Some((id, path, line_no)) = parse_breakpoint_location_line(line) {
                if line_no != linenumber {
                    continue;
                }
                let normalized = {
                    let parsed_path = std::path::Path::new(&path);
                    if parsed_path.is_absolute() {
                        parsed_path.to_path_buf()
                    } else {
                        self.config.codebase_dir.join(parsed_path)
                    }
                };

                if normalized == target
                    || normalized
                        .to_string_lossy()
                        .ends_with(target.to_string_lossy().as_ref())
                {
                    ids.push(id);
                }
            }
        }

        Ok(ids)
    }

    async fn execute_run(&mut self) -> Result<DebuggerResponse> {
        let response = self
            .execute_command("run", Some(DebuggerState::Running))
            .await?;

        // Per spec, when run stops at a breakpoint, return the full normal response.
        if response.debugger_state == DebuggerState::StoppedAtBreakpoint {
            return self.full_snapshot_response().await;
        }

        Ok(response)
    }

    async fn execute_gdbserver(
        &mut self,
        ip: String,
        port: u16,
        pid: i64,
    ) -> Result<DebuggerResponse> {
        if pid <= 0 {
            self.debugger_state = DebuggerState::GdbServerFailedToAttach;
            self.last_error = "pid must be > 0".to_string();
            return Ok(self.base_response().with_error(self.last_error.clone()));
        }

        let _ = self.stop_gdbserver_process().await;

        let endpoint = format!("{ip}:{port}");
        let mut command = tokio::process::Command::new("gdbserver");
        command
            .arg("--attach")
            .arg(&endpoint)
            .arg(pid.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let child_result = command.spawn();
        let mut child = match child_result {
            Ok(child) => child,
            Err(err) => {
                self.debugger_state = DebuggerState::GdbServerFailedToAttach;
                self.last_error = format!("failed to start gdbserver: {err}");
                return Ok(self.base_response().with_error(self.last_error.clone()));
            }
        };

        sleep(Duration::from_millis(100)).await;
        match child.try_wait().map_err(OpenMcpGdbError::Io)? {
            Some(status) => {
                self.debugger_state = DebuggerState::GdbServerFailedToAttach;
                self.last_error = format!("gdbserver exited early with status: {status}");
                Ok(self.base_response().with_error(self.last_error.clone()))
            }
            None => {
                self.gdbserver_child = Some(child);
                self.debugger_state = DebuggerState::GdbServerAttached;
                self.last_error.clear();
                Ok(self.base_response())
            }
        }
    }

    async fn execute_target_remote(&mut self, ip: String, port: u16) -> Result<DebuggerResponse> {
        // Remote attach still requires a local gdb process. If it is not started yet,
        // start gdb with configured executable for symbols before target remote.
        if self.executable_path.is_none() {
            match self.backend.start(&self.config.executable_path).await {
                Ok(_) => {
                    self.executable_path = Some(self.config.executable_path.clone());
                    self.debugger_state = DebuggerState::Attached;
                    self.last_error.clear();
                }
                Err(err) => {
                    self.debugger_state = DebuggerState::FailedToAttach;
                    self.last_error = err.to_string();
                    return Ok(self.base_response().with_error(self.last_error.clone()));
                }
            }
        }

        self.execute_command(
            &format!("target remote {ip}:{port}"),
            Some(DebuggerState::Attached),
        )
        .await
    }

    async fn execute_with_full_snapshot(
        &mut self,
        command: &str,
        fallback_state: DebuggerState,
    ) -> Result<DebuggerResponse> {
        // Do not auto-restart backend after quit; return current state instead.
        if self.debugger_state == DebuggerState::NotAttached
            || self.debugger_state == DebuggerState::FailedToAttach
            || self.debugger_state == DebuggerState::GdbServerFailedToAttach
        {
            return Ok(self.base_response());
        }
        let response = self.execute_command(command, Some(fallback_state)).await?;
        match response.debugger_state {
            DebuggerState::Error
            | DebuggerState::SigSegv
            | DebuggerState::SigAbrt
            | DebuggerState::SigBus
            | DebuggerState::SigFpe
            | DebuggerState::SigIll
            | DebuggerState::SigTrap
            | DebuggerState::SigTerm
            | DebuggerState::SigKill
            | DebuggerState::Exited
            | DebuggerState::Running
            | DebuggerState::NotAttached
            | DebuggerState::FailedToAttach
            | DebuggerState::GdbServerFailedToAttach => return Ok(response),
            _ => {}
        }
        self.full_snapshot_response().await
    }

    async fn execute_interrupt(&mut self) -> Result<DebuggerResponse> {
        if self.debugger_state == DebuggerState::NotAttached
            || self.debugger_state == DebuggerState::FailedToAttach
            || self.debugger_state == DebuggerState::GdbServerFailedToAttach
            || self.debugger_state == DebuggerState::Exited
        {
            return Ok(self.base_response());
        }

        let interrupt_result = self.backend.interrupt().await;
        if let Err(err) = interrupt_result {
            self.debugger_state = DebuggerState::Error;
            self.last_error = err.to_string();
            return Ok(self.base_response().with_error(self.last_error.clone()));
        }

        // Force prompt resynchronization after SIGINT before issuing follow-up queries.
        let sync_result = self.backend.exec("printf \"\"").await;
        if let Err(err) = sync_result {
            self.debugger_state = DebuggerState::Error;
            self.last_error = err.to_string();
            return Ok(self.base_response().with_error(self.last_error.clone()));
        }

        self.debugger_state = DebuggerState::StoppedAtStepping;
        self.last_error.clear();
        self.full_snapshot_response().await
    }

    async fn full_snapshot_response(&mut self) -> Result<DebuggerResponse> {
        let mut response = self.base_response();
        response.variable_list = Some(self.collect_variable_list().await?);
        let (backtrace, current_func) = self.collect_backtrace(true).await?;
        response.backtrace = Some(backtrace);
        response.current_func = current_func;
        let code = self.collect_current_code().await?;
        response.current_code_path = code.0;
        response.current_code_line = code.1.and_then(|line| i64::try_from(line).ok());
        response.current_code = code.2.map(|lines| self.transform_current_code(lines));
        Ok(response)
    }

    async fn variable_list_response(&mut self) -> Result<DebuggerResponse> {
        if self.debugger_state == DebuggerState::NotAttached
            || self.debugger_state == DebuggerState::FailedToAttach
            || self.debugger_state == DebuggerState::GdbServerFailedToAttach
            || self.debugger_state == DebuggerState::Exited
        {
            let mut response = self.base_response();
            response.variable_list = Some(BTreeMap::new());
            return Ok(response);
        }
        if self.debugger_state == DebuggerState::Running {
            let _ = self.backend.interrupt().await;
            let _ = self.backend.exec("printf \"\"").await;
            self.debugger_state = DebuggerState::StoppedAtStepping;
        }
        self.recover_error_state_without_restart();
        let mut response = self.base_response();
        response.variable_list = Some(self.collect_variable_list().await?);
        Ok(response)
    }

    async fn full_backtrace_response(&mut self) -> Result<DebuggerResponse> {
        // Do not auto-restart backend after quit; return current state instead.
        if self.debugger_state == DebuggerState::NotAttached
            || self.debugger_state == DebuggerState::FailedToAttach
            || self.debugger_state == DebuggerState::GdbServerFailedToAttach
        {
            return Ok(self.base_response());
        }
        self.recover_error_state_without_restart();
        let mut response = self.base_response();
        let (backtrace, current_func) = self.collect_backtrace(true).await?;
        response.backtrace = Some(backtrace);
        response.current_func = current_func;
        Ok(response)
    }

    async fn list_breakpoint_response(&mut self) -> Result<DebuggerResponse> {
        if self.debugger_state == DebuggerState::NotAttached
            || self.debugger_state == DebuggerState::FailedToAttach
            || self.debugger_state == DebuggerState::GdbServerFailedToAttach
        {
            return Ok(self.base_response());
        }
        if self.debugger_state == DebuggerState::Running {
            let _ = self.backend.interrupt().await;
            let _ = self.backend.exec("printf \"\"").await;
            self.debugger_state = DebuggerState::StoppedAtStepping;
        }
        self.recover_error_state_without_restart();

        let output = self.backend.exec("info breakpoints").await;
        match output {
            Ok(output) => {
                let lines: Vec<String> = output
                    .lines()
                    .map(str::trim_end)
                    .filter(|line| !line.is_empty())
                    .map(|line| line.strip_prefix("(gdb) ").unwrap_or(line).to_string())
                    .collect();

                let mut response = self.base_response();
                response.breakpoints = Some(lines);
                Ok(response)
            }
            Err(err) => {
                self.last_error = err.to_string();
                self.debugger_state = DebuggerState::Error;
                Ok(self.base_response().with_error(self.last_error.clone()))
            }
        }
    }

    async fn current_code_response(&mut self) -> Result<DebuggerResponse> {
        // Do not auto-restart backend after quit; return current state instead.
        if self.debugger_state == DebuggerState::NotAttached
            || self.debugger_state == DebuggerState::FailedToAttach
            || self.debugger_state == DebuggerState::GdbServerFailedToAttach
        {
            return Ok(self.base_response());
        }
        if self.debugger_state == DebuggerState::Running {
            let _ = self.backend.interrupt().await;
            let _ = self.backend.exec("printf \"\"").await;
            self.debugger_state = DebuggerState::StoppedAtStepping;
        }
        self.recover_error_state_without_restart();
        let mut response = self.base_response();
        let code = self.collect_current_code().await?;
        response.current_code_path = code.0;
        response.current_code_line = code.1.and_then(|line| i64::try_from(line).ok());
        response.current_code = code.2.map(|lines| self.transform_current_code(lines));
        if response.current_code_path.is_none()
            && response.current_code_line.is_none()
            && response.current_code.is_none()
            && response.error.is_empty()
        {
            response.error = "no current frame".to_string();
        }
        Ok(response)
    }

    async fn collect_variable_list(&mut self) -> Result<BTreeMap<String, String>> {
        let mut variables = BTreeMap::new();

        for variable in self
            .watched_variables
            .iter()
            .take(self.config.display_variable_list)
        {
            let output = self.backend.exec(&format!("print {variable}")).await;
            match output {
                Ok(output) => {
                    let value = if looks_like_gdb_error(&output) {
                        let details = normalized_command_output(&output)
                            .unwrap_or_else(|| "gdb print failed".to_string());
                        format!("<error: {details}>")
                    } else {
                        normalize_gdb_value(&output)
                    };
                    variables.insert(variable.clone(), value);
                }
                Err(err) => {
                    variables.insert(variable.clone(), format!("<error: {err}>"));
                }
            }
        }

        Ok(variables)
    }

    async fn collect_backtrace(
        &mut self,
        full: bool,
    ) -> Result<(BTreeMap<String, (String, String)>, Option<String>)> {
        let command = if full { "backtrace full" } else { "backtrace" };
        let mut output = self.backend.exec(command).await?;
        let mut backtrace = BTreeMap::new();
        parse_backtrace_lines(&output, self.config.display_backtrace, &mut backtrace);

        // Some targets or GDB settings can yield sparse/empty "backtrace full" output.
        // Fall back to plain backtrace to keep frame info available to MCP clients.
        if full && backtrace.is_empty() {
            output = self.backend.exec("backtrace").await?;
            parse_backtrace_lines(&output, self.config.display_backtrace, &mut backtrace);
        }

        let current_func = if let Some((func, _)) = backtrace.get("0") {
            Some(func.clone())
        } else {
            let mut best: Option<(u64, String)> = None;
            for (frame_key, (func, _)) in &backtrace {
                if let Ok(frame_num) = frame_key.parse::<u64>() {
                    match &best {
                        Some((best_num, _)) if frame_num >= *best_num => {}
                        _ => {
                            best = Some((frame_num, func.clone()));
                        }
                    }
                }
            }
            best.map(|(_, func)| func)
                .or_else(|| backtrace.values().next().map(|(func, _)| func.clone()))
        };
        Ok((backtrace, current_func))
    }

    async fn collect_current_code(
        &mut self,
    ) -> Result<(
        Option<String>,
        Option<u64>,
        Option<BTreeMap<u64, String>>,
    )> {
        let frame = self.backend.exec("frame").await?;
        let (path, line) = parse_path_and_line(&frame);

        let mut code_lines = BTreeMap::new();
        if let Some(line) = line {
            let before = self.config.display_lines_before_current as u64;
            let after = self.config.display_lines_after_current as u64;
            let start = std::cmp::max(1, line.saturating_sub(before));
            let end = line + after;
            let list_output = self
                .backend
                .exec(&format!("list {start},{end}"))
                .await?;
            for raw_line in list_output.lines() {
                if let Some((number, source)) = parse_gdb_list_line(raw_line) {
                    code_lines.insert(number, source.to_string());
                }
            }
        }

        let current_code = if code_lines.is_empty() {
            None
        } else {
            Some(code_lines)
        };

        let normalized_path = path.map(|raw| {
            let path_obj = std::path::Path::new(&raw);
            if path_obj.is_absolute() {
                raw
            } else {
                self.config
                    .codebase_dir
                    .join(path_obj)
                    .to_string_lossy()
                    .to_string()
            }
        });

        Ok((normalized_path, line, current_code))
    }

    fn base_response(&self) -> DebuggerResponse {
        let mut response = DebuggerResponse::new(self.debugger_state);
        if !self.last_error.is_empty() {
            response.error = self.last_error.clone();
        }
        response
    }

    fn update_state_from_output(&mut self, output: &str, fallback_state: Option<DebuggerState>) {
        let lower = output.to_ascii_lowercase();

        // GDB command failures should be surfaced as Error state.
        if lower.contains("undefined command")
            || lower.contains("ambiguous command")
            || lower.contains("not recognized")
            || lower.contains("a syntax error in expression")
            || lower.contains("cannot find bounds of current function")
            || lower.contains("no symbol")
            || lower.contains("unknown thread")
            || lower.contains("no frame at level")
            || lower.contains("no source file named")
            || lower.contains("no breakpoint at")
            || lower.contains("no breakpoint number")
            || lower.contains("error:")
        {
            self.debugger_state = DebuggerState::Error;
            return;
        }

        // Signal detection: check both explicit signal names and descriptive messages.
        if lower.contains("sigsegv") || lower.contains("segmentation fault") {
            self.debugger_state = DebuggerState::SigSegv;
            return;
        }
        if lower.contains("sigabrt")
            || (lower.contains("signal received") && lower.contains("sigabrt"))
            || lower.contains("program received signal sigabrt")
        {
            self.debugger_state = DebuggerState::SigAbrt;
            return;
        }
        if lower.contains("sigbus") || lower.contains("bus error") {
            self.debugger_state = DebuggerState::SigBus;
            return;
        }
        if lower.contains("sigfpe") || lower.contains("floating point exception") {
            self.debugger_state = DebuggerState::SigFpe;
            return;
        }
        if lower.contains("sigill") || lower.contains("illegal instruction") {
            self.debugger_state = DebuggerState::SigIll;
            return;
        }
        if lower.contains("sigtrap") {
            self.debugger_state = DebuggerState::SigTrap;
            return;
        }
        if lower.contains("sigterm") {
            self.debugger_state = DebuggerState::SigTerm;
            return;
        }
        if lower.contains("sigkill") {
            self.debugger_state = DebuggerState::SigKill;
            return;
        }
        if lower.contains("sigint") && !lower.contains("breakpoint") {
            self.debugger_state = DebuggerState::StoppedAtStepping;
            return;
        }

        // Generic "program received signal" pattern (catches any unhandled signal).
        if lower.contains("program received signal") {
            self.debugger_state = DebuggerState::Error;
            return;
        }
        if lower.contains("terminated with signal") {
            self.debugger_state = DebuggerState::Error;
            return;
        }

        // Breakpoint, watchpoint, and catchpoint stop detection.
        if contains_breakpoint_stop(output) {
            self.debugger_state = DebuggerState::StoppedAtBreakpoint;
            return;
        }
        if contains_breakpoint_creation(output) {
            return;
        }
        if lower.contains("watchpoint") || lower.contains("hardware watchpoint") {
            self.debugger_state = DebuggerState::StoppedAtBreakpoint;
            return;
        }
        if lower.contains("catchpoint") {
            self.debugger_state = DebuggerState::StoppedAtBreakpoint;
            return;
        }

        // Program termination and exit detection.

        // Program termination and exit detection.
        if lower.contains("exited normally")
            || lower.contains("exited with code")
            || lower.contains("exited abnormally")
            || (lower.contains("inferior") && lower.contains("exited"))
        {
            self.debugger_state = DebuggerState::Exited;
            return;
        }

        // Program not running / no context detection.
        if lower.contains("no stack") || lower.contains("no registers") {
            self.debugger_state = DebuggerState::Exited;
            return;
        }
        if lower.contains("the program is not being run")
            || lower.contains("no inferior")
            || lower.contains("the program has no registers now")
        {
            self.debugger_state = DebuggerState::NotAttached;
            return;
        }

        // Running state detection from GDB output.
        if lower.contains("continuing") || lower.contains("starting program") {
            self.debugger_state = DebuggerState::Running;
            return;
        }

        // User interrupt detection.
        if lower.contains("interrupted") {
            self.debugger_state = DebuggerState::StoppedAtBreakpoint;
            return;
        }

        // Memory access error detection.
        if lower.contains("cannot access memory") {
            self.debugger_state = DebuggerState::Error;
            return;
        }

        // Detaching / process finished detection.
        if lower.contains("detaching") || lower.contains("process finished") {
            self.debugger_state = DebuggerState::NotAttached;
            return;
        }

        if (lower.contains("inferior") && lower.contains("killed"))
            || lower.contains("program received signal sigkill")
            || lower.contains("terminated with signal sigkill")
        {
            self.debugger_state = DebuggerState::SigKill;
            return;
        }

        if let Some(state) = fallback_state {
            self.debugger_state = state;
        }
    }

    fn transform_current_code(&self, lines: BTreeMap<u64, String>) -> CurrentCodePayload {
        if self.config.display_join_current_code {
            let mut joined = String::new();
            let mut first = true;
            for (line_no, source) in lines {
                if !first {
                    joined.push('\n');
                }
                first = false;
                joined.push_str(&format!("{line_no} | {source}"));
            }
            CurrentCodePayload::Joined(joined)
        } else {
            CurrentCodePayload::Lines(lines)
        }
    }

    async fn stop_gdbserver_process(&mut self) -> Result<()> {
        if let Some(mut child) = self.gdbserver_child.take() {
            let _ = child.kill().await;
        }
        Ok(())
    }

    async fn execute_kill(&mut self) -> Result<DebuggerResponse> {
        if self.executable_path.is_none() {
            return Ok(self.base_response());
        }

        if self.debugger_state == DebuggerState::Running {
            let _ = self.backend.interrupt().await;
            let _ = self.backend.exec("printf \"\"").await;
            self.debugger_state = DebuggerState::StoppedAtStepping;
        }

        let output = self.backend.exec("kill").await;
        match output {
            Ok(output) => {
                let mut merged_output = output.clone();
                if output.to_ascii_lowercase().contains("kill the program being debugged") {
                    let confirm = self.backend.exec("y").await;
                    match confirm {
                        Ok(confirm_output) => {
                            merged_output.push('\n');
                            merged_output.push_str(&confirm_output);
                        }
                        Err(err) => {
                            self.debugger_state = DebuggerState::Error;
                            self.last_error = err.to_string();
                            return Ok(self.base_response().with_error(self.last_error.clone()));
                        }
                    }
                }

                self.update_state_from_output(&merged_output, None);
                let lower = merged_output.to_ascii_lowercase();
                if lower.contains("the program is not being run") || lower.contains("no inferior") {
                    self.debugger_state = DebuggerState::NotAttached;
                } else if lower.contains("killed")
                    || lower.contains("sigkill")
                    || lower.contains("terminated with signal")
                {
                    self.debugger_state = DebuggerState::SigKill;
                }
                self.last_error.clear();
                Ok(self.base_response())
            }
            Err(err) => {
                self.debugger_state = DebuggerState::Error;
                self.last_error = err.to_string();
                Ok(self.base_response().with_error(self.last_error.clone()))
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum BreakpointAction {
    Enable,
    Disable,
}

impl BreakpointAction {
    fn command_for_id(&self, id: &str) -> String {
        match self {
            Self::Enable => format!("enable {id}"),
            Self::Disable => format!("disable {id}"),
        }
    }
}

fn normalize_gdb_value(output: &str) -> String {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Some((_, rhs)) = trimmed.split_once('=') {
        return rhs.trim().to_string();
    }
    return trimmed.to_string();
}

fn normalized_command_output(output: &str) -> Option<String> {
    let trimmed = output.trim();
    let stripped = trimmed.strip_prefix("(gdb) ").unwrap_or(trimmed);
    if stripped.is_empty() {
        None
    } else {
        Some(stripped.to_string())
    }
}

fn looks_like_gdb_error(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("undefined command")
        || lower.contains("ambiguous command")
        || lower.contains("not recognized")
        || lower.contains("a syntax error in expression")
        || lower.contains("cannot find bounds of current function")
        || lower.contains("no symbol")
        || lower.contains("unknown thread")
        || lower.contains("no frame at level")
        || lower.contains("no source file named")
        || lower.contains("no breakpoint at")
        || lower.contains("no breakpoint number")
        || lower.contains("error:")
        || lower.contains("cannot access memory")
}

fn is_recoverable_command_error(lower_error: &str) -> bool {
    lower_error.contains("no symbol")
        || lower_error.contains("unknown thread")
        || lower_error.contains("no frame at level")
        || lower_error.contains("no source file named")
        || lower_error.contains("no breakpoint at")
        || lower_error.contains("no breakpoint number")
        || lower_error.contains("a syntax error in expression")
        || lower_error.contains("cannot find bounds of current function")
}

fn contains_breakpoint_stop(output: &str) -> bool {
    output.lines().any(|line| {
        let trimmed = line.trim_start().to_ascii_lowercase();
        if let Some(rest) = trimmed.strip_prefix("breakpoint ") {
            return rest.contains(',');
        }
        false
    })
}

fn contains_breakpoint_creation(output: &str) -> bool {
    output.lines().any(|line| {
        let trimmed = line.trim_start().to_ascii_lowercase();
        if let Some(rest) = trimmed.strip_prefix("breakpoint ") {
            return rest.contains(" at 0x")
                || rest.contains(": file ")
                || rest.contains("pending");
        }
        false
    })
}

fn parse_path_and_line(frame_output: &str) -> (Option<String>, Option<u64>) {
    for line in frame_output.lines() {
        if let Some(at_idx) = line.find(" at ") {
            let segment = line[(at_idx + 4)..].trim();
            if let Some((path, line_number)) = segment.rsplit_once(':') {
                if let Ok(number) = line_number.parse::<u64>() {
                    return (Some(path.to_string()), Some(number));
                }
            }
        }
    }
    (None, None)
}

fn parse_gdb_list_line(line: &str) -> Option<(u64, &str)> {
    let trimmed = line.trim_start();
    let digits_len = trimmed
        .chars()
        .take_while(|char| char.is_ascii_digit())
        .count();
    if digits_len == 0 {
        return None;
    }

    let (digits, rest) = trimmed.split_at(digits_len);
    let number = digits.parse::<u64>().ok()?;
    // Preserve original source indentation: strip only the first field separator.
    let source = if let Some(stripped) = rest.strip_prefix('\t') {
        stripped
    } else if let Some(stripped) = rest.strip_prefix(' ') {
        stripped
    } else {
        rest
    };
    Some((number, source))
}

fn parse_breakpoint_location_line(line: &str) -> Option<(String, String, u64)> {
    let trimmed = line.trim_start();
    let mut parts = trimmed.split_whitespace();
    let id = parts.next()?;
    if !id.chars().all(|char| char.is_ascii_digit() || char == '.') {
        return None;
    }

    let at_idx = trimmed.rfind(" at ")?;
    let location = trimmed[(at_idx + 4)..].trim();
    let (path, line_no) = location.rsplit_once(':')?;
    let line_number = line_no.parse::<u64>().ok()?;
    Some((id.to_string(), path.to_string(), line_number))
}

fn parse_backtrace_lines(
    output: &str,
    limit: usize,
    backtrace: &mut BTreeMap<String, (String, String)>,
) {
    for line in output.lines() {
        if backtrace.len() >= limit {
            break;
        }
        if let Some((frame_number, function, location)) = parse_backtrace_frame_line(line) {
            backtrace.insert(frame_number.to_string(), (function, location));
        }
    }
}

fn parse_backtrace_frame_line(line: &str) -> Option<(u64, String, String)> {
    let hash_idx = line.find('#')?;
    let frame_section = &line[(hash_idx + 1)..];
    let digits_len = frame_section
        .chars()
        .take_while(|char| char.is_ascii_digit())
        .count();
    if digits_len == 0 {
        return None;
    }

    let frame_number = frame_section[..digits_len].parse::<u64>().ok()?;
    let mut rest = frame_section[digits_len..].trim_start();

    if let Some(index) = rest.find(" in ") {
        rest = &rest[(index + 4)..];
    } else if let Some(stripped) = rest.strip_prefix("in ") {
        rest = stripped;
    }

    let mut function = rest.split_whitespace().next().unwrap_or("unknown");
    if function == "in" {
        function = rest
            .split_whitespace()
            .nth(1)
            .unwrap_or("unknown");
    }
    if let Some((name, _)) = function.split_once('(') {
        function = name;
    }

    let location = line
        .rsplit_once(" at ")
        .map(|(_, loc)| loc.trim().to_string())
        .unwrap_or_default();

    Some((frame_number, function.to_string(), location))
}

#[cfg(test)]
mod parse_tests {
    use super::{parse_backtrace_frame_line, parse_gdb_list_line};

    #[test]
    fn test_parse_gdb_list_line_preserves_space_indentation() {
        let parsed = parse_gdb_list_line("23\t    simulator_init(&g_sim, rows, cols, seed);");
        assert!(parsed.is_some(), "line should parse");
        let (line_no, source) = parsed.expect("parsed line");
        assert_eq!(line_no, 23);
        assert_eq!(source, "    simulator_init(&g_sim, rows, cols, seed);");
    }

    #[test]
    fn test_parse_gdb_list_line_preserves_tab_indentation() {
        let parsed = parse_gdb_list_line("24\t\trobot_init(&g_robot, &g_sim);");
        assert!(parsed.is_some(), "line should parse");
        let (line_no, source) = parsed.expect("parsed line");
        assert_eq!(line_no, 24);
        assert_eq!(source, "\trobot_init(&g_robot, &g_sim);");
    }

    #[test]
    fn test_parse_backtrace_frame_line_with_address_and_in() {
        let parsed = parse_backtrace_frame_line(
            "#0  0x00005555555551cb in compute_pi (value=3) at /tmp/main.c:55",
        );
        assert!(parsed.is_some(), "frame should parse");
        let (frame_no, function, location) = parsed.expect("parsed frame");
        assert_eq!(frame_no, 0);
        assert_eq!(function, "compute_pi");
        assert_eq!(location, "/tmp/main.c:55");
    }

    #[test]
    fn test_list_breakpoint_strips_gdb_prompt() {
        let input = "(gdb) Num     Type           Disp Enb Address            What\n1       breakpoint     keep y   0x00005555555551cb in main at src/main.c:10\n";
        let lines: Vec<String> = input
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
            .map(|line| line.strip_prefix("(gdb) ").unwrap_or(line).to_string())
            .collect();

        assert_eq!(lines.len(), 2);
        assert!(!lines[0].starts_with("(gdb)"));
        assert!(!lines[1].starts_with("(gdb)"));
        assert_eq!(
            lines[0],
            "Num     Type           Disp Enb Address            What"
        );
    }
}
