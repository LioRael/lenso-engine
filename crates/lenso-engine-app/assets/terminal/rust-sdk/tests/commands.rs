use futures::executor::block_on;
use lenso_cli_support as sdk;
use lenso_kernel::{NativeStreamItem, NativeStreamSession};

mod typed {
    /// Typed greeting
    #[lenso_cli_support::command(name = "hello")]
    async fn hello(
        #[arg(default = "world")] name: String,
        #[arg(default = "2")] repeat: u32,
        suffix: Option<String>,
        loud: bool,
    ) -> Result<String, String> {
        if repeat == 0 {
            return Err("repeat must be positive".into());
        }
        let value = format!("Hello, {name}{}", suffix.unwrap_or_default()).repeat(repeat as usize);
        Ok(if loud { value.to_uppercase() } else { value })
    }
}
mod streaming {
    use std::sync::atomic::{AtomicBool, Ordering};
    pub static DROPPED: AtomicBool = AtomicBool::new(false);
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            DROPPED.store(true, Ordering::SeqCst);
        }
    }
    #[lenso_cli_support::command]
    async fn progress(#[context] context: lenso_cli_support::CommandContext) {
        let _guard = Guard;
        context.text("started");
        futures::future::pending::<()>().await;
    }
}
mod synchronous {
    #[lenso_cli_support::command]
    fn hello() -> &'static str {
        "sync works"
    }
}
fn open(command: sdk::Command, args: &str) -> Box<dyn NativeStreamSession> {
    block_on(sdk::execute(
        command,
        "test",
        sdk::ExecuteOpen {
            id: "test".into(),
            arguments_json: args.to_owned().try_into().unwrap(),
            output_format: sdk::OutputFormat::Text,
        },
    ))
    .unwrap()
}
fn text(session: &dyn NativeStreamSession) -> String {
    match block_on(session.receive()).unwrap() {
        NativeStreamItem::Message(value) => {
            value.downcast::<sdk::ExecuteMessage>().unwrap().content
        }
        other => panic!("expected message: {other:?}"),
    }
}
#[test]
fn typed_macro_catalog_defaults_and_arguments_use_the_existing_contract() {
    let catalog = block_on(sdk::catalog(typed::command(), "test"))
        .unwrap()
        .unwrap();
    assert_eq!(catalog.commands[0].summary, "Typed greeting");
    assert_eq!(catalog.commands[0].path, ["hello"]);
    let session = open(
        typed::command(),
        r#"{"name":"Ada","repeat":"1","loud":true,"suffix":"!"}"#,
    );
    assert_eq!(text(session.as_ref()), "HELLO, ADA!\n");
    assert!(matches!(
        block_on(session.receive()).unwrap(),
        NativeStreamItem::Terminal(Ok(()))
    ));
    assert!(block_on(session.receive()).is_err());
    assert_eq!(
        text(open(typed::command(), "{}").as_ref()),
        "Hello, worldHello, world\n"
    );
}
#[test]
fn invalid_typed_values_and_business_errors_stay_domain_errors() {
    for (args, invalid) in [(r#"{"repeat":"bad"}"#, true), (r#"{"repeat":"0"}"#, false)] {
        let session = open(typed::command(), args);
        let NativeStreamItem::Terminal(Err(error)) = block_on(session.receive()).unwrap() else {
            panic!("expected domain error")
        };
        let error = error.downcast::<sdk::ExecuteError>().unwrap();
        assert!(if invalid {
            matches!(*error, sdk::ExecuteError::InvalidArguments)
        } else {
            matches!(*error, sdk::ExecuteError::ExecutionFailed { .. })
        });
    }
}
#[test]
fn progress_arrives_before_completion_and_cancel_drops_the_command_future() {
    use std::sync::atomic::Ordering;
    let session = open(streaming::command(), "{}");
    assert_eq!(text(session.as_ref()), "started\n");
    assert!(!streaming::DROPPED.load(Ordering::SeqCst));
    session.cancel();
    assert!(streaming::DROPPED.load(Ordering::SeqCst));
    assert!(block_on(session.receive()).is_err());
}
#[test]
fn synchronous_macros_and_existing_builders_remain_supported() {
    assert_eq!(
        text(open(synchronous::command(), "{}").as_ref()),
        "sync works\n"
    );
    let command = sdk::Command::new("old", "Old builder")
        .string_arg("name", "world")
        .run(|args, output| {
            output.text(&args["name"]);
            Ok(())
        });
    assert_eq!(text(open(command, "{}").as_ref()), "world\n");
}
#[test]
fn output_is_bounded_even_for_async_commands() {
    let command = sdk::Command::new("flood", "Bounded").run_async(|_, context| {
        Box::pin(async move {
            for _ in 0..300 {
                context.text("line");
            }
            Ok(())
        })
    });
    let session = open(command, "{}");
    for _ in 0..256 {
        assert_eq!(text(session.as_ref()), "line\n");
    }
    let NativeStreamItem::Terminal(Err(error)) = block_on(session.receive()).unwrap() else {
        panic!("expected limit")
    };
    assert!(matches!(
        *error.downcast::<sdk::ExecuteError>().unwrap(),
        sdk::ExecuteError::OutputLimitExceeded
    ));
}

mod required {
    #[lenso_cli_support::command]
    fn echo(name: String) -> String {
        name
    }
}
#[test]
fn required_and_unknown_arguments_fail_before_execution() {
    let catalog = block_on(sdk::catalog(required::command(), "test"))
        .unwrap()
        .unwrap();
    assert!(catalog.commands[0].parameters[0].required);
    for args in ["{}", r#"{"name":"Ada","extra":"unknown"}"#] {
        let result = block_on(sdk::execute(
            required::command(),
            "test",
            sdk::ExecuteOpen {
                id: "test".into(),
                arguments_json: args.to_owned().try_into().unwrap(),
                output_format: sdk::OutputFormat::Text,
            },
        ));
        assert!(matches!(
            result,
            Err(sdk::CommandProviderExecuteInvocationError::Domain(
                sdk::ExecuteError::InvalidArguments
            ))
        ));
    }
}
