use std::fs;

use tempfile::TempDir;

use super::*;

const THREAD_ID: &str = "019f52ac-7a9f-7fd1-8dda-e775ef950785";

fn target(cwd: &Path) -> ResumeTarget {
    ResumeTarget {
        thread_id: THREAD_ID.to_owned(),
        title: "Main feature implementation".to_owned(),
        cwd: Some(cwd.to_path_buf()),
        source: Some("desktop".to_owned()),
        parent_thread_id: None,
        status: TaskStatus::Completed,
        archived: false,
    }
}

fn executable_script(path: &Path, script: &str) {
    fs::write(path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }
}

fn executable(path: &Path) {
    executable_script(path, "#!/bin/sh\nexit 0\n");
}

fn fixture() -> (TempDir, ResumeTarget, LaunchContext) {
    let temp = tempfile::tempdir().unwrap();
    let monitor_cwd = temp.path().join("monitor root");
    let task_cwd = temp.path().join("task root --quoted");
    let codex_home = monitor_cwd.join("relative home");
    let bin = temp.path().join("bin");
    fs::create_dir_all(&monitor_cwd).unwrap();
    fs::create_dir_all(&task_cwd).unwrap();
    fs::create_dir_all(&codex_home).unwrap();
    fs::create_dir_all(&bin).unwrap();
    executable(&bin.join("codex"));
    executable(&bin.join("zellij"));
    let context = LaunchContext::new(
        PathBuf::from("relative home"),
        None,
        env::join_paths([&bin]).unwrap(),
        monitor_cwd,
        true,
    );
    (temp, target(&task_cwd), context)
}

#[test]
fn validates_only_canonical_lowercase_uuid() {
    assert!(is_canonical_thread_uuid(THREAD_ID));
    assert!(!is_canonical_thread_uuid(
        "019F52AC-7A9F-7FD1-8DDA-E775EF950785"
    ));
    assert!(!is_canonical_thread_uuid(
        "019f52ac7a9f-7fd1-8dda-e775ef950785"
    ));
    assert!(!is_canonical_thread_uuid(
        "019f52ac-7a9f-7fd1-8dda-e775ef95078z"
    ));
    assert!(!is_canonical_thread_uuid("latest"));
}

#[test]
fn eligibility_rejects_subagents_active_archived_and_bad_cwd() {
    let temp = tempfile::tempdir().unwrap();
    let mut candidate = target(temp.path());
    assert_eq!(check_eligibility(&candidate), Ok(temp.path()));

    candidate.source = Some("SUBAGENT".to_owned());
    assert_eq!(
        check_eligibility(&candidate),
        Err(EligibilityError::Subagent)
    );
    candidate.source = Some("desktop".to_owned());
    candidate.parent_thread_id = Some("parent".to_owned());
    assert_eq!(check_eligibility(&candidate), Ok(temp.path()));
    candidate.parent_thread_id = None;
    candidate.status = TaskStatus::WaitingInput;
    assert_eq!(
        check_eligibility(&candidate),
        Err(EligibilityError::Active(TaskStatus::WaitingInput))
    );
    candidate.archived = true;
    assert_eq!(
        check_eligibility(&candidate),
        Err(EligibilityError::Archived)
    );
    candidate.archived = false;
    candidate.status = TaskStatus::Completed;
    candidate.cwd = None;
    assert_eq!(
        check_eligibility(&candidate),
        Err(EligibilityError::MissingCwd)
    );
    candidate.cwd = Some(PathBuf::from("relative"));
    assert!(matches!(
        check_eligibility(&candidate),
        Err(EligibilityError::RelativeCwd(_))
    ));
    candidate.cwd = Some(temp.path().join("missing"));
    assert!(matches!(
        check_eligibility(&candidate),
        Err(EligibilityError::CwdNotFound(_))
    ));
    let file = temp.path().join("file");
    fs::write(&file, "not a directory").unwrap();
    candidate.cwd = Some(file);
    assert!(matches!(
        check_eligibility(&candidate),
        Err(EligibilityError::CwdNotDirectory(_))
    ));
}

#[test]
fn launch_plan_preserves_argv_boundaries_and_environment() {
    let (_temp, target, context) = fixture();
    let plan = prepare_zellij_launch(&target, &context, &ZellijOptions::default()).unwrap();
    assert!(plan.command.program.is_absolute());
    assert_eq!(plan.command.program, plan.zellij_bin);

    let args = &plan.command.args;
    assert_eq!(&args[..2], ["action", "new-pane"]);
    assert!(args.contains(&OsString::from("--floating")));
    assert!(args.contains(&OsString::from("90%")));
    assert!(args.contains(&target.cwd.as_ref().unwrap().as_os_str().to_owned()));

    let separator = args.iter().position(|arg| arg == "--").unwrap();
    let command = &args[separator + 1..];
    assert_eq!(command[0], ENV_BIN);
    assert_eq!(command[1], env_assignment("PATH", &context.path));
    assert_eq!(
        command[2],
        env_assignment(
            "CODEX_HOME",
            context.monitor_cwd.join("relative home").as_os_str()
        )
    );
    assert!(Path::new(&command[3]).is_absolute());
    assert_eq!(
        &command[4..],
        [
            OsString::from("resume"),
            OsString::from("--cd"),
            target.cwd.as_ref().unwrap().as_os_str().to_owned(),
            OsString::from(THREAD_ID),
        ]
    );
}

#[test]
fn resume_command_does_not_require_a_zellij_context() {
    let (_temp, target, mut context) = fixture();
    context.in_zellij = false;

    let plan = prepare_resume_command(&target, &context).unwrap();

    assert_eq!(plan.thread_id, THREAD_ID);
    assert_eq!(plan.cwd, target.cwd.as_ref().unwrap().as_path());
    assert_eq!(plan.command.program, Path::new(ENV_BIN));
    assert_eq!(plan.command.args[0], env_assignment("PATH", &context.path));
    assert_eq!(
        plan.command.args[1],
        env_assignment(
            "CODEX_HOME",
            context.monitor_cwd.join("relative home").as_os_str()
        )
    );
    assert!(Path::new(&plan.command.args[2]).is_absolute());
    assert_eq!(
        &plan.command.args[3..],
        [
            OsString::from("resume"),
            OsString::from("--cd"),
            target.cwd.as_ref().unwrap().as_os_str().to_owned(),
            OsString::from(THREAD_ID),
        ]
    );
}

#[test]
fn copied_resume_command_uses_the_target_shell_without_inheriting_path() {
    let (_temp, target, mut context) = fixture();
    context.in_zellij = false;

    let plan = prepare_resume_copy_command(&target, &context).unwrap();
    let rendered = render_posix_resume_command(&plan).unwrap();

    assert_eq!(plan.thread_id, THREAD_ID);
    assert_eq!(plan.cwd, target.cwd.as_ref().unwrap().as_path());
    assert_eq!(plan.command.program, Path::new("codex"));
    assert_eq!(
        plan.command.args,
        resume_arguments(target.cwd.as_ref().unwrap(), THREAD_ID)
    );
    assert!(rendered.starts_with("CODEX_HOME="));
    assert!(rendered.contains(" codex resume --cd "));
    assert!(rendered.ends_with(THREAD_ID));
    assert!(!rendered.contains("PATH="));
    assert!(!rendered.contains(context.path.to_string_lossy().as_ref()));
}

#[cfg(unix)]
#[test]
fn copied_resume_command_round_trips_environment_and_arguments_through_sh() {
    let (_temp, target, context) = fixture();
    let bin = env::split_paths(&context.path).next().unwrap();
    executable_script(
        &bin.join("codex"),
        "#!/bin/sh\nprintf '%s\\n' \"$CODEX_HOME\" \"$@\"\n",
    );
    let rendered =
        render_posix_resume_command(&prepare_resume_copy_command(&target, &context).unwrap())
            .unwrap();

    let output = Command::new("/bin/sh")
        .args(["-c", &rendered])
        .env("PATH", &context.path)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        [
            context
                .monitor_cwd
                .join("relative home")
                .to_string_lossy()
                .as_ref(),
            "resume",
            "--cd",
            target.cwd.as_ref().unwrap().to_string_lossy().as_ref(),
            THREAD_ID,
        ]
    );
}

#[test]
fn zellij_launch_reuses_the_exact_resume_command() {
    let (_temp, target, context) = fixture();
    let resume = prepare_resume_command(&target, &context).unwrap();
    let zellij = prepare_zellij_launch(&target, &context, &ZellijOptions::default()).unwrap();
    let separator = zellij
        .command
        .args
        .iter()
        .position(|arg| arg == "--")
        .unwrap();
    let expected = std::iter::once(resume.command.program.as_os_str().to_owned())
        .chain(resume.command.args)
        .collect::<Vec<_>>();

    assert_eq!(&zellij.command.args[separator + 1..], expected);
}

#[test]
fn renders_every_resume_argument_as_a_posix_shell_word() {
    let plan = ResumeCopyPlan {
        thread_id: THREAD_ID.to_owned(),
        cwd: PathBuf::from("/tmp/project '雪 $;`cwd`"),
        codex_home: PathBuf::from("/tmp/codex home/'quoted 雪"),
        command: CommandPlan {
            program: PathBuf::from("/opt/codex $;`fast`"),
            args: vec![
                OsString::from("resume"),
                OsString::from("--cd"),
                OsString::from("/tmp/project '雪 $;`cwd`"),
                OsString::from(THREAD_ID),
            ],
        },
    };

    assert_eq!(
        render_posix_resume_command(&plan).unwrap(),
        r#"CODEX_HOME='/tmp/codex home/'"'"'quoted 雪' '/opt/codex $;`fast`' resume --cd '/tmp/project '"'"'雪 $;`cwd`' 019f52ac-7a9f-7fd1-8dda-e775ef950785"#
    );
}

#[test]
fn copied_resume_command_rejects_controls_and_bidi_overrides() {
    let mut plan = ResumeCopyPlan {
        thread_id: THREAD_ID.to_owned(),
        cwd: PathBuf::from("/tmp"),
        codex_home: PathBuf::from("/tmp"),
        command: CommandPlan {
            program: PathBuf::from(ENV_BIN),
            args: vec![OsString::from("line\nbreak")],
        },
    };

    assert!(matches!(
        render_posix_resume_command(&plan),
        Err(PrepareError::UnrepresentableShellCommand)
    ));

    plan.command.args[0] = OsString::from("safe-looking\u{202e}txt");
    assert!(matches!(
        render_posix_resume_command(&plan),
        Err(PrepareError::UnrepresentableShellCommand)
    ));
}

#[cfg(unix)]
#[test]
fn copied_resume_command_rejects_non_utf8_arguments() {
    use std::os::unix::ffi::OsStringExt;

    let plan = ResumeCopyPlan {
        thread_id: THREAD_ID.to_owned(),
        cwd: PathBuf::from("/tmp"),
        codex_home: PathBuf::from("/tmp"),
        command: CommandPlan {
            program: PathBuf::from(ENV_BIN),
            args: vec![OsString::from_vec(b"invalid-\xff".to_vec())],
        },
    };

    assert!(matches!(
        render_posix_resume_command(&plan),
        Err(PrepareError::UnrepresentableShellCommand)
    ));
}

#[test]
fn relative_codex_override_is_resolved_against_monitor_cwd() {
    let (_temp, target, mut context) = fixture();
    let tools = context.monitor_cwd.join("tools");
    fs::create_dir(&tools).unwrap();
    executable(&tools.join("custom-codex"));
    context.codex_bin = Some(PathBuf::from("tools/custom-codex"));

    let plan = prepare_zellij_launch(&target, &context, &ZellijOptions::default()).unwrap();
    let separator = plan
        .command
        .args
        .iter()
        .position(|arg| arg == "--")
        .unwrap();
    assert_eq!(
        PathBuf::from(&plan.command.args[separator + 4]),
        fs::canonicalize(tools.join("custom-codex")).unwrap()
    );
    let copy = prepare_resume_copy_command(&target, &context).unwrap();
    assert_eq!(
        copy.command.program,
        fs::canonicalize(tools.join("custom-codex")).unwrap()
    );
    assert!(
        !render_posix_resume_command(&copy)
            .unwrap()
            .contains("PATH=")
    );
}

#[test]
fn non_floating_and_close_on_exit_have_exact_flags() {
    let (_temp, target, context) = fixture();
    let options = ZellijOptions {
        floating: false,
        width_percent: 80,
        height_percent: 70,
        close_on_exit: true,
    };
    let plan = prepare_zellij_launch(&target, &context, &options).unwrap();
    assert!(!plan.command.args.contains(&OsString::from("--floating")));
    assert!(!plan.command.args.contains(&OsString::from("--width")));
    assert!(!plan.command.args.contains(&OsString::from("--height")));
    assert!(
        plan.command
            .args
            .contains(&OsString::from("--close-on-exit"))
    );
}

#[test]
fn rejects_invalid_dimensions_and_non_zellij_context() {
    let (_temp, target, mut context) = fixture();
    let options = ZellijOptions {
        width_percent: 0,
        ..ZellijOptions::default()
    };
    assert!(matches!(
        prepare_zellij_launch(&target, &context, &options),
        Err(PrepareError::InvalidDimension {
            name: "width",
            value: 0
        })
    ));

    context.in_zellij = false;
    assert!(matches!(
        prepare_zellij_launch(&target, &context, &ZellijOptions::default()),
        Err(PrepareError::NotInZellij)
    ));
}

#[test]
fn focus_preflight_does_not_require_codex_or_task_state() {
    let (_temp, _target, mut context) = fixture();
    context.codex_home = PathBuf::from("missing-home");
    context.codex_bin = Some(PathBuf::from("missing-codex"));

    let zellij = prepare_zellij_focus(&context).unwrap();
    assert!(zellij.is_absolute());
    assert_eq!(zellij.file_name().and_then(OsStr::to_str), Some("zellij"));
}

#[test]
fn pane_name_removes_controls_bidi_and_truncates_by_display_width() {
    let title = "hello\n\x1b[31m\u{202e}world 主要功能".repeat(8);
    let name = pane_name(THREAD_ID, &title);
    assert!(name.starts_with("codex 019f52ac - "));
    assert!(!name.chars().any(char::is_control));
    assert!(!name.chars().any(is_bidi_control));
    assert!(UnicodeWidthStr::width(name.as_str()) <= PANE_NAME_MAX_WIDTH);
    assert!(name.ends_with("..."));
}

#[test]
fn parses_only_terminal_pane_ids() {
    assert_eq!(
        parse_created_pane_id(b"terminal_42\n").unwrap().as_str(),
        "terminal_42"
    );
    assert!(parse_created_pane_id(b"plugin_42\n").is_err());
    assert!(parse_created_pane_id(b"terminal_1 extra").is_err());
}

#[test]
fn parses_flat_and_nested_zellij_pane_lists() {
    let output = br#"[
          {"id":2,"is_plugin":false,"title":"shell"},
          {"id":4,"is_plugin":true,"title":"plugin"},
          {"tab":{"id":99,"panes":[
            {"id":7,"isPlugin":false},
            {"pane_id":"2","is_plugin":false},
            {"id":8,"is_plugin":true}
          ]}}
        ]"#;
    let panes = parse_listed_pane_ids(output).unwrap();
    assert_eq!(
        panes.iter().map(PaneId::as_str).collect::<Vec<_>>(),
        ["terminal_2", "terminal_7"]
    );
    assert!(parse_listed_pane_ids(b"not json").is_err());
}

#[test]
fn executes_new_pane_and_focuses_an_existing_terminal() {
    let (_temp, target, context) = fixture();
    let plan = prepare_zellij_launch(&target, &context, &ZellijOptions::default()).unwrap();
    executable_script(
        &plan.zellij_bin,
        "#!/bin/sh\nif [ \"$2\" = \"new-pane\" ]; then printf 'terminal_42\\n'; exit 0; fi\nexit 2\n",
    );
    assert_eq!(
        execute_zellij_launch(&plan).unwrap(),
        LaunchResult::Created {
            pane_id: PaneId::parse("terminal_42").unwrap()
        }
    );

    executable_script(
        &plan.zellij_bin,
        "#!/bin/sh\nif [ \"$2\" = \"list-panes\" ]; then printf '[{\"id\":42,\"is_plugin\":false}]'; exit 0; fi\nif [ \"$2\" = \"focus-pane-id\" ] && [ \"$3\" = \"terminal_42\" ]; then exit 0; fi\nexit 3\n",
    );
    assert_eq!(
        focus_existing_pane(&plan.zellij_bin, &PaneId::parse("terminal_42").unwrap()).unwrap(),
        FocusResult::Focused
    );
}

#[test]
fn missing_panes_and_rejected_actions_are_structured() {
    let (_temp, target, context) = fixture();
    let plan = prepare_zellij_launch(&target, &context, &ZellijOptions::default()).unwrap();
    executable_script(
        &plan.zellij_bin,
        "#!/bin/sh\nif [ \"$2\" = \"list-panes\" ]; then printf '[]'; exit 0; fi\nprintf 'bad\\noutput\\033[31m' >&2\nexit 7\n",
    );
    assert_eq!(
        focus_existing_pane(&plan.zellij_bin, &PaneId::parse("terminal_99").unwrap()).unwrap(),
        FocusResult::Missing
    );
    let error = execute_zellij_launch(&plan).unwrap_err().to_string();
    assert!(error.contains("exit 7"));
    assert!(error.contains("bad output[31m"));
    assert!(!error.contains('\n'));
    assert!(!error.contains('\u{1b}'));
}
