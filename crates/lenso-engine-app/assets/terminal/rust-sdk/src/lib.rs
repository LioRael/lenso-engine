mod generated;
use futures::future::LocalBoxFuture;
pub use generated::*;
pub use lenso_cli_macros::command;
use lenso_kernel::RuntimeFailure;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, VecDeque},
    rc::Rc,
    task::{Poll, Waker},
};
pub type Context = lenso_kernel::InvocationContext;
pub type CatalogFuture = lenso_kernel::NativeRequestFuture<CommandProviderCatalog>;
pub type ExecuteFuture = futures::future::LocalBoxFuture<
    'static,
    Result<Box<dyn lenso_kernel::NativeStreamSession>, CommandProviderExecuteInvocationError>,
>;
pub type CommandFuture = futures::future::LocalBoxFuture<'static, Result<(), CommandError>>;
type Handler = fn(&BTreeMap<String, String>, &mut Output) -> Result<(), String>;
type AsyncHandler = fn(BTreeMap<String, String>, CommandContext) -> CommandFuture;
#[derive(Clone, Copy, Debug)]
enum Execution {
    Sync(Handler),
    Async(AsyncHandler),
}
#[derive(Clone, Debug)]
struct Argument {
    default: Option<String>,
    required: bool,
    flag: bool,
    description: String,
}
#[derive(Clone, Debug)]
pub struct Command {
    name: String,
    description: String,
    arguments: BTreeMap<String, Argument>,
    handler: Execution,
}
impl Command {
    pub fn new(name: &str, description: &str) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            arguments: BTreeMap::new(),
            handler: Execution::Sync(|_, _| Ok(())),
        }
    }
    pub fn string_arg(self, name: &str, default: &str) -> Self {
        self.argument(name, Some(default), false, false, "")
    }
    pub fn argument(
        mut self,
        name: &str,
        default: Option<&str>,
        required: bool,
        flag: bool,
        description: &str,
    ) -> Self {
        self.arguments.insert(
            name.into(),
            Argument {
                default: default.map(str::to_owned),
                required,
                flag,
                description: description.into(),
            },
        );
        self
    }
    pub fn run(mut self, handler: Handler) -> Self {
        self.handler = Execution::Sync(handler);
        self
    }
    pub fn run_async(mut self, handler: AsyncHandler) -> Self {
        self.handler = Execution::Async(handler);
        self
    }
}
#[derive(Debug)]
pub enum CommandError {
    InvalidArguments,
    Failed(String),
}
pub fn parse_argument<T: std::str::FromStr>(value: &str) -> Result<T, CommandError> {
    value.parse().map_err(|_| CommandError::InvalidArguments)
}
/// Return either text, no value, or a Result of those from a command function.
pub trait CommandReturn {
    fn finish(self, context: &CommandContext) -> Result<(), CommandError>;
}
impl CommandReturn for () {
    fn finish(self, _: &CommandContext) -> Result<(), CommandError> {
        Ok(())
    }
}
impl CommandReturn for String {
    fn finish(self, context: &CommandContext) -> Result<(), CommandError> {
        context.text(self);
        Ok(())
    }
}
impl CommandReturn for &str {
    fn finish(self, context: &CommandContext) -> Result<(), CommandError> {
        context.text(self);
        Ok(())
    }
}
impl<T: CommandReturn, E: std::fmt::Display> CommandReturn for Result<T, E> {
    fn finish(self, context: &CommandContext) -> Result<(), CommandError> {
        self.map_err(|error| CommandError::Failed(error.to_string()))?
            .finish(context)
    }
}
#[derive(Debug, Default)]
pub struct Output {
    messages: VecDeque<ExecuteMessage>,
    bytes: usize,
    overflow: bool,
}
impl Output {
    pub fn text(&mut self, text: impl Into<String>) {
        self.push(OutputKind::Stdout, text.into());
    }
    pub fn error(&mut self, text: impl Into<String>) {
        self.push(OutputKind::Stderr, text.into());
    }
    fn push(&mut self, kind: OutputKind, text: String) {
        let content = format!("{text}\n");
        self.bytes = self.bytes.saturating_add(content.len());
        if self.messages.len() >= 256 || self.bytes > 16 * 1024 * 1024 {
            self.overflow = true;
            return;
        }
        self.messages.push_back(ExecuteMessage {
            content,
            content_type: ContentType::Text,
            kind,
        });
    }
}
#[derive(Debug, Default)]
struct Shared {
    output: RefCell<Output>,
    cancelled: Cell<bool>,
    waker: RefCell<Option<Waker>>,
}
/// Invocation-local output. Dropping/cancelling the Stream drops its command future.
#[derive(Clone, Debug)]
pub struct CommandContext(Rc<Shared>);
impl CommandContext {
    pub fn text(&self, text: impl Into<String>) {
        self.push(OutputKind::Stdout, text.into());
    }
    pub fn error(&self, text: impl Into<String>) {
        self.push(OutputKind::Stderr, text.into());
    }
    pub fn cancelled(&self) -> bool {
        self.0.cancelled.get()
    }
    fn push(&self, kind: OutputKind, text: String) {
        if self.cancelled() {
            return;
        }
        self.0.output.borrow_mut().push(kind, text);
        if let Some(waker) = self.0.waker.borrow_mut().take() {
            waker.wake();
        }
    }
}
pub fn catalog(command: Command, id: &str) -> CatalogFuture {
    let parameters = command
        .arguments
        .iter()
        .map(|(key, arg)| CommandParameter {
            id: key.clone(),
            kind: if arg.flag {
                ParameterKind::Flag
            } else {
                ParameterKind::Option
            },
            long: Some(Some(key.clone())),
            short: None,
            value_name: None,
            description: arg.description.clone(),
            required: arg.required,
            multiple: false,
            choices: vec![],
        })
        .collect();
    let response = CatalogResponse {
        commands: vec![CommandDefinition {
            id: id.into(),
            path: command.name.split(' ').map(str::to_owned).collect(),
            summary: command.description.clone(),
            description: command.description,
            parameters,
            output_formats: vec![OutputFormat::Text],
        }],
    };
    Box::pin(async move { Ok(Ok(response)) })
}
fn arguments(command: &Command, request: &ExecuteOpen) -> Result<BTreeMap<String, String>, ()> {
    let args: BTreeMap<String, serde_json::Value> =
        serde_json::from_str(request.arguments_json.as_str()).map_err(|_| ())?;
    if args.keys().any(|key| !command.arguments.contains_key(key)) {
        return Err(());
    }
    let mut merged = BTreeMap::new();
    for (key, arg) in &command.arguments {
        let value = match args.get(key) {
            Some(serde_json::Value::String(value)) if !arg.flag => Some(value.clone()),
            Some(serde_json::Value::Bool(value)) if arg.flag => Some(value.to_string()),
            Some(_) => return Err(()),
            None => arg
                .default
                .clone()
                .or_else(|| arg.flag.then(|| "false".into())),
        };
        if let Some(value) = value {
            merged.insert(key.clone(), value);
        } else if arg.required {
            return Err(());
        }
    }
    Ok(merged)
}
pub fn execute(command: Command, id: &str, request: ExecuteOpen) -> ExecuteFuture {
    if request.id != id {
        return Box::pin(async {
            Err(CommandProviderExecuteInvocationError::Domain(
                ExecuteError::NotFound,
            ))
        });
    }
    let Ok(args) = arguments(&command, &request) else {
        return Box::pin(async {
            Err(CommandProviderExecuteInvocationError::Domain(
                ExecuteError::InvalidArguments,
            ))
        });
    };
    let shared = Rc::new(Shared::default());
    let context = CommandContext(shared.clone());
    let future = match command.handler {
        Execution::Async(handler) => handler(args, context),
        Execution::Sync(handler) => Box::pin(async move {
            handler(&args, &mut context.0.output.borrow_mut()).map_err(CommandError::Failed)
        }) as CommandFuture,
    };
    let session = Session(Rc::new(State {
        shared,
        future: RefCell::new(Some(future)),
        error: RefCell::new(None),
        terminal: Cell::new(false),
    }));
    Box::pin(async move { Ok(Box::new(session) as Box<dyn lenso_kernel::NativeStreamSession>) })
}
struct State {
    shared: Rc<Shared>,
    future: RefCell<Option<CommandFuture>>,
    error: RefCell<Option<ExecuteError>>,
    terminal: Cell<bool>,
}
struct Session(Rc<State>);
impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandSession").finish_non_exhaustive()
    }
}
impl lenso_kernel::NativeStreamSession for Session {
    fn send(
        &self,
        _: Box<dyn std::any::Any>,
    ) -> LocalBoxFuture<'static, Result<(), RuntimeFailure>> {
        Box::pin(async {
            Err(RuntimeFailure::ProtocolViolation {
                capability: CAPABILITY_ID,
            })
        })
    }
    fn receive(
        &self,
    ) -> LocalBoxFuture<'static, Result<lenso_kernel::NativeStreamItem, RuntimeFailure>> {
        let state = self.0.clone();
        Box::pin(futures::future::poll_fn(move |cx| {
            if state.shared.cancelled.get() || state.terminal.get() {
                return Poll::Ready(Err(RuntimeFailure::AdmissionClosed));
            }
            *state.shared.waker.borrow_mut() = Some(cx.waker().clone());
            let completion = state.future.borrow_mut().as_mut().and_then(|future| {
                match future.as_mut().poll(cx) {
                    Poll::Ready(result) => Some(result),
                    Poll::Pending => None,
                }
            });
            if let Some(result) = completion {
                state.future.borrow_mut().take();
                *state.error.borrow_mut() = result.err().map(|error| match error {
                    CommandError::InvalidArguments => ExecuteError::InvalidArguments,
                    CommandError::Failed(message) => ExecuteError::ExecutionFailed {
                        payload: ExecutionFailedPayload {
                            reason_code: "command_failed".into(),
                            message: message.chars().take(4096).collect(),
                            details_json: "{}".to_owned().try_into().expect("JSON object"),
                        },
                    },
                });
            }
            if state.shared.output.borrow().overflow {
                state.future.borrow_mut().take();
                *state.error.borrow_mut() = Some(ExecuteError::OutputLimitExceeded);
            }
            if let Some(message) = state.shared.output.borrow_mut().messages.pop_front() {
                return Poll::Ready(Ok(lenso_kernel::NativeStreamItem::Message(Box::new(
                    message,
                ))));
            }
            if state.future.borrow().is_some() {
                return Poll::Pending;
            }
            state.terminal.set(true);
            Poll::Ready(Ok(lenso_kernel::NativeStreamItem::Terminal(
                match state.error.borrow_mut().take() {
                    None => Ok(()),
                    Some(error) => Err(Box::new(error)),
                },
            )))
        }))
    }
    fn close_send(&self) -> LocalBoxFuture<'static, Result<(), RuntimeFailure>> {
        Box::pin(async { Ok(()) })
    }
    fn cancel(&self) {
        self.0.shared.cancelled.set(true);
        self.0.future.borrow_mut().take();
        self.0.shared.output.borrow_mut().messages.clear();
        if let Some(waker) = self.0.shared.waker.borrow_mut().take() {
            waker.wake();
        }
    }
}
