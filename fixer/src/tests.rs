// Tests are written by an agent against policy.rs and plan.rs.
#[allow(unused_imports)]
use super::*;
use crate::plan::{plan, spawner, split_name_pid};
use crate::policy::{denied, Policy, DENY_NAMES};
use crate::{Action, Alert, Mode, Subject, Target, Verdict};

const NOW: f64 = 1_000_000.0;

fn pol(mode: Mode) -> Policy {
    Policy { mode, now: NOW, paused_until: 0.0, licensed: true, allow: vec![], extra_deny: vec![], recent_auto: vec![] }
}

fn tgt(name: &str, cmd: &str, age: f64) -> Target {
    Target { pid: 1234, name: name.into(), age_secs: age, cmdline: cmd.into(), service: None, private_mb: 100.0 }
}

fn old(name: &str) -> Target { tgt(name, "", 600.0) }

#[test]
fn deny_names_refused_for_terminate_and_trim() {
    for n in DENY_NAMES {
        let up = n.to_uppercase();
        let t = old(&up);
        assert!(denied(&t, &Action::Terminate, &[]).is_some(), "terminate {}", n);
        assert!(denied(&t, &Action::Trim, &[]).is_some(), "trim {}", n);
    }
}

#[test]
fn dwm_refused_by_name_any_case() {
    assert!(denied(&old("DWM.exe"), &Action::Terminate, &[]).is_some());
    assert!(denied(&old("dwm.EXE"), &Action::Trim, &[]).is_some());
}

#[test]
fn snifrig_binaries_refused() {
    assert!(denied(&old("snifrig-tray.exe"), &Action::Trim, &[]).is_some());
    assert!(denied(&old("Snifrig-Fix.exe"), &Action::Terminate, &[]).is_some());
}

#[test]
fn gpu_arbiter_script_refused_under_python() {
    let t = tgt("python.exe", r"C:\py\python.exe X:\gpu_arbiter.py --serve", 600.0);
    assert!(denied(&t, &Action::Terminate, &[]).is_some());
    let t2 = tgt("python.exe", r"python.exe C:\GPU_Arbiter.PY", 600.0);
    assert!(denied(&t2, &Action::Trim, &[]).is_some());
}

#[test]
fn ferryman_cmdline_refused() {
    let t = tgt("node.exe", "node.exe C:\\ferry\\Ferryman\\index.js", 600.0);
    assert!(denied(&t, &Action::Terminate, &[]).is_some());
}

#[test]
fn fix_deny_by_name_and_cmdline() {
    let extra = vec!["myapp.exe".to_string(), "secret-tool".to_string()];
    assert!(denied(&old("MyApp.exe"), &Action::Trim, &extra).is_some());
    let t = tgt("cmd.exe", "cmd.exe /c secret-tool run", 600.0);
    assert!(denied(&t, &Action::Trim, &extra).is_some());
    assert!(denied(&old("other.exe"), &Action::Trim, &extra).is_none());
}

#[test]
fn svchost_terminate_refused_restart_passes_deny() {
    let t = old("svchost.exe");
    assert!(denied(&t, &Action::Terminate, &[]).is_some());
    assert!(denied(&t, &Action::RestartService("Dhcp".into()), &[]).is_none());
}

#[test]
fn system_pids_refused() {
    let mut t = old("whatever.exe");
    t.pid = 0;
    assert!(denied(&t, &Action::Trim, &[]).is_some());
    t.pid = 4;
    assert!(denied(&t, &Action::Trim, &[]).is_some());
    t.pid = std::process::id();
    assert!(denied(&t, &Action::Trim, &[]).is_some());
}

#[test]
fn young_process_refused_old_allowed() {
    let p = pol(Mode::Auto);
    assert!(matches!(p.decide(&Action::Trim, Some(&tgt("a.exe", "", 119.0))), Verdict::Refuse(_)));
    assert_eq!(p.decide(&Action::Trim, Some(&tgt("a.exe", "", 121.0))), Verdict::Execute);
}

#[test]
fn mode_off_refuses_everything() {
    let p = pol(Mode::Off);
    for a in [Action::Report, Action::Trim, Action::LowerPriority, Action::Terminate, Action::RestartService("Dhcp".into())].iter() {
        assert!(matches!(p.decide(a, Some(&old("a.exe"))), Verdict::Refuse(_)), "{:?}", a);
        assert!(matches!(p.decide(a, None), Verdict::Refuse(_)), "{:?}", a);
    }
}

#[test]
fn report_is_always_log_only() {
    for m in [Mode::DryRun, Mode::Ask, Mode::Auto].iter() {
        let mut p = pol(*m);
        assert_eq!(p.decide(&Action::Report, None), Verdict::LogOnly("report only".into()));
        assert!(matches!(p.decide(&Action::Report, Some(&old("x.exe"))), Verdict::LogOnly(_)));
        p.licensed = false;
        p.paused_until = NOW + 60.0;
        assert!(matches!(p.decide(&Action::Report, Some(&old("x.exe"))), Verdict::LogOnly(_)));
    }
}

#[test]
fn paused_is_log_only() {
    let mut p = pol(Mode::Auto);
    p.paused_until = NOW + 100.0;
    assert_eq!(p.decide(&Action::Trim, Some(&old("a.exe"))), Verdict::LogOnly("fixing is paused".into()));
}

#[test]
fn dry_run_is_log_only() {
    assert_eq!(pol(Mode::DryRun).decide(&Action::Trim, Some(&old("a.exe"))), Verdict::LogOnly("dry run".into()));
}

#[test]
fn unlicensed_ask_is_log_only() {
    let mut p = pol(Mode::Ask);
    p.licensed = false;
    assert!(matches!(p.decide(&Action::Trim, Some(&old("a.exe"))), Verdict::LogOnly(_)));
}

#[test]
fn licensed_ask_queues() {
    assert_eq!(pol(Mode::Ask).decide(&Action::Terminate, Some(&old("a.exe"))), Verdict::Queue);
}

#[test]
fn auto_mild_executes() {
    let p = pol(Mode::Auto);
    assert_eq!(p.decide(&Action::Trim, Some(&old("a.exe"))), Verdict::Execute);
    assert_eq!(p.decide(&Action::LowerPriority, Some(&old("a.exe"))), Verdict::Execute);
}

#[test]
fn auto_terminate_queues_unless_allowlisted() {
    let p = pol(Mode::Auto);
    assert_eq!(p.decide(&Action::Terminate, Some(&old("a.exe"))), Verdict::Queue);
    let mut by_name = pol(Mode::Auto);
    by_name.allow = vec!["a.exe".into()];
    assert_eq!(by_name.decide(&Action::Terminate, Some(&old("A.exe"))), Verdict::Execute);
    let mut by_cmd = pol(Mode::Auto);
    by_cmd.allow = vec!["mytool".into()];
    let t = tgt("node.exe", "node.exe C:\\tools\\MyTool\\run.js", 600.0);
    assert_eq!(by_cmd.decide(&Action::Terminate, Some(&t)), Verdict::Execute);
}

#[test]
fn auto_rate_limit_recent() {
    let mut p = pol(Mode::Auto);
    p.recent_auto = vec![NOW - 10.0, NOW - 600.0, NOW - 3000.0];
    assert_eq!(p.decide(&Action::Trim, Some(&old("a.exe"))), Verdict::Queue);
    p.recent_auto = vec![NOW - 3601.0, NOW - 3700.0, NOW - 4000.0];
    assert_eq!(p.decide(&Action::Trim, Some(&old("a.exe"))), Verdict::Execute);
}

#[test]
fn terminate_without_target_refused() {
    assert!(matches!(pol(Mode::Auto).decide(&Action::Terminate, None), Verdict::Refuse(_)));
}

#[test]
fn approve_check_ignores_mode_and_rate() {
    let mut p = pol(Mode::Off);
    p.recent_auto = vec![NOW, NOW, NOW];
    assert!(p.approve_check(&Action::Terminate, &old("a.exe")).is_ok());
    p.mode = Mode::DryRun;
    assert!(p.approve_check(&Action::Trim, &old("a.exe")).is_ok());
}

#[test]
fn approve_check_still_gates() {
    let p = pol(Mode::Auto);
    assert!(p.approve_check(&Action::Terminate, &old("DWM.exe")).is_err());
    assert!(p.approve_check(&Action::Trim, &tgt("a.exe", "", 60.0)).is_err());
    let mut paused = pol(Mode::Auto);
    paused.paused_until = NOW + 5.0;
    assert!(paused.approve_check(&Action::Trim, &old("a.exe")).is_err());
    let mut unlic = pol(Mode::Auto);
    unlic.licensed = false;
    assert!(unlic.approve_check(&Action::Trim, &old("a.exe")).is_err());
}

fn alert(key: &str, msg: &str) -> Alert {
    Alert { t: "2026-10-07T00:00:00Z".into(), key: key.into(), msg: msg.into() }
}

#[test]
fn pm_and_ph_keys_plan_terminate() {
    let i = plan(&alert("pm:python.exe#1234", ""));
    assert_eq!(i.subject, Subject::Pid { name: "python.exe".into(), pid: 1234 });
    assert_eq!(i.action, Action::Terminate);
    let j = plan(&alert("ph:node.exe#99", ""));
    assert_eq!(j.subject, Subject::Pid { name: "node.exe".into(), pid: 99 });
    assert_eq!(j.action, Action::Terminate);
}

#[test]
fn bad_pid_keys_report() {
    for k in ["pm:foo#x", "pm:foo#0"].iter() {
        let i = plan(&alert(k, ""));
        assert_eq!(i.action, Action::Report, "{}", k);
        assert_eq!(i.subject, Subject::None, "{}", k);
    }
}

#[test]
fn split_name_pid_parses() {
    assert_eq!(split_name_pid("python.exe#1234"), Some(("python.exe".to_string(), 1234)));
    assert_eq!(split_name_pid("#5"), None);
    assert_eq!(split_name_pid("a#0"), None);
}

#[test]
fn spawn_burst_in_message_plans_terminate() {
    let msg = "Spawn burst watch: bash.exe#4321 (started by wsl.exe) made 40 processes in 5 s (...)";
    let i = plan(&alert("tag:Toke", msg));
    assert_eq!(i.subject, Subject::Pid { name: "bash.exe".into(), pid: 4321 });
    assert_eq!(i.action, Action::Terminate);
}

#[test]
fn spawner_token_trailing_punctuation() {
    let want = Some(("bash.exe".to_string(), 4321));
    assert_eq!(spawner("Spawn burst watch: bash.exe#4321. rest"), want);
    assert_eq!(spawner("Spawn burst watch: bash.exe#4321, rest"), want);
    assert_eq!(spawner("Spawn burst watch: bash.exe#4321;"), want);
}

#[test]
fn x_avail_trims_top_private() {
    let i = plan(&alert("x:avail", ""));
    assert_eq!(i.subject, Subject::TopPrivate(1));
    assert_eq!(i.action, Action::Trim);
}

#[test]
fn system_keys_report() {
    for k in ["x:commit", "g:paged", "big:FMfn", "self"].iter() {
        let i = plan(&alert(k, ""));
        assert_eq!(i.action, Action::Report, "{}", k);
        assert_eq!(i.subject, Subject::None, "{}", k);
    }
}

#[test]
fn pm_key_wins_over_spawner() {
    let msg = "Spawn burst watch: bash.exe#4321 (started by wsl.exe) made 40 processes";
    let i = plan(&alert("pm:python.exe#1234", msg));
    assert_eq!(i.subject, Subject::Pid { name: "python.exe".into(), pid: 1234 });
    assert_eq!(i.action, Action::Terminate);
}

#[test]
fn action_label_parse_round_trip() {
    let all = vec![
        Action::Report,
        Action::Trim,
        Action::LowerPriority,
        Action::Terminate,
        Action::RestartService("Dhcp".into()),
        Action::RestartService("Wlan AutoConfig".into()),
    ];
    for a in all.iter() {
        assert_eq!(Action::parse(&a.label()).as_ref(), Some(a), "{:?}", a);
    }
    assert_eq!(Action::RestartService("Dhcp".into()).label(), "restart-service:Dhcp");
    assert_eq!(Action::parse("bogus"), None);
}

#[test]
fn mode_parse_and_as_str() {
    for (s, m) in [("off", Mode::Off), ("dry-run", Mode::DryRun), ("ask", Mode::Ask), ("auto", Mode::Auto)].iter() {
        assert_eq!(Mode::parse(s), Some(*m));
        assert_eq!(m.as_str(), *s);
        assert_eq!(Mode::parse(&format!("  {} \n", s)), Some(*m));
    }
    assert_eq!(Mode::parse("AUTO"), None);
    assert_eq!(Mode::parse("bogus"), None);
}
