use crate::cmdline::{CommandLineError, parse};

fn strings(entries: &[&str]) -> Vec<Vec<u8>> {
    entries
        .iter()
        .map(|entry| entry.as_bytes().to_vec())
        .collect()
}

#[test]
fn kernel_parameters_are_consumed_and_the_rest_reach_init() {
    let parameters = parse(
        b"root=/dev/vda rootfstype=ext4 rootwait ro rw console=ttyS0,115200 console=ttyAMA0 \
          init=/bin/init quiet vm.overcommit=1 single TERM=vt100 MODE=x MODE=y -- -a b=c",
    )
    .unwrap();
    assert_eq!(parameters.root.as_deref(), Some(&b"/dev/vda"[..]));
    assert_eq!(
        parameters.root_filesystem_type.as_deref(),
        Some(&b"ext4"[..])
    );
    assert!(!parameters.read_only_root);
    assert_eq!(parameters.console.as_deref(), Some(&b"ttyAMA0"[..]));
    assert_eq!(parameters.init.as_deref(), Some(&b"/bin/init"[..]));
    assert_eq!(parameters.log_level, Some(4));
    // 模块参数忽略；裸词与 `--` 之后的全部词进 argv；`name=value` 进环境且同名后者覆盖。
    assert_eq!(parameters.init_arguments, strings(&["single", "-a", "b=c"]));
    assert_eq!(
        parameters.init_environment,
        strings(&["HOME=/", "TERM=vt100", "MODE=y"])
    );
}

#[test]
fn quotes_group_whitespace_and_are_stripped_from_values() {
    let parameters = parse(br#"GREETING="hello world" "quoted word" init="/bin/my init""#).unwrap();
    assert_eq!(
        parameters.init_environment,
        strings(&["HOME=/", "TERM=linux", "GREETING=hello world"])
    );
    assert_eq!(parameters.init_arguments, strings(&["quoted word"]));
    assert_eq!(parameters.init.as_deref(), Some(&b"/bin/my init"[..]));
}

#[test]
fn init_argument_and_environment_limits_match_linux() {
    let arguments = (0..33)
        .map(|index| format!("a{index}"))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(
        parse(arguments.as_bytes()),
        Err(CommandLineError::TooManyArguments)
    );
    // 环境上限包含 HOME/TERM 两个初始项。
    let environment = (0..31)
        .map(|index| format!("E{index}=1"))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(
        parse(environment.as_bytes()),
        Err(CommandLineError::TooManyEnvironment)
    );
    assert_eq!(parse(b"loglevel=x"), Err(CommandLineError::InvalidLogLevel));
}
