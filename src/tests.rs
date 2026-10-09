use super::*;

fn series(pts: &[(f64, f64)]) -> Series {
    let mut s = Series::new();
    for (i, &(t, x)) in pts.iter().enumerate() {
        s.push(t, x, i as u64);
    }
    s
}

fn spaced(n: usize, step: f64, f: impl Fn(f64) -> f64) -> Vec<(f64, f64)> {
    (0..n)
        .map(|i| {
            let t = i as f64 * step;
            (t, f(t))
        })
        .collect()
}

// ---------- json_str / json_num ----------

#[test]
fn json_str_reads_string_values() {
    let s = r#"{"name":"snifrig","ver":"1.2"}"#;
    assert_eq!(json_str(s, "name").as_deref(), Some("snifrig"));
    assert_eq!(json_str(s, "ver").as_deref(), Some("1.2"));
}

#[test]
fn json_str_missing_key_is_none() {
    assert_eq!(json_str(r#"{"a":"b"}"#, "zz"), None);
}

#[test]
fn json_str_empty_value() {
    assert_eq!(json_str(r#"{"a":"","b":"x"}"#, "a").as_deref(), Some(""));
}

#[test]
fn json_str_ignores_non_string_value() {
    assert_eq!(json_str(r#"{"n":12}"#, "n"), None);
}

#[test]
fn json_num_reads_digits() {
    assert_eq!(json_num(r#"{"count":42,"x":1}"#, "count"), 42);
    assert_eq!(json_num(r#"{"x":1,"count":7}"#, "count"), 7);
}

#[test]
fn json_num_missing_or_non_numeric_is_zero() {
    assert_eq!(json_num(r#"{"a":1}"#, "count"), 0);
    assert_eq!(json_num(r#"{"count":"abc"}"#, "count"), 0);
}

// ---------- esc ----------

#[test]
fn esc_escapes_backslash_and_quote() {
    assert_eq!(esc(r"C:\tmp"), r"C:\\tmp");
    assert_eq!(esc(r#"say "hi""#), r#"say \"hi\""#);
    assert_eq!(esc(r#"\""#), r#"\\\""#);
    assert_eq!(esc("plain"), "plain");
}

// ---------- script_names ----------

#[test]
fn script_names_keeps_program_and_script_basenames() {
    assert_eq!(script_names(r"C:\tools\python.exe C:\tools\run.py --key=abc"), "python.exe run.py");
    assert_eq!(script_names(r#"cmd /c "C:\Program Files\app\start.bat""#), "cmd start.bat");
}

#[test]
fn script_names_drops_secret_arguments() {
    assert_eq!(script_names("python run.py --password hunter2"), "python run.py");
}

#[test]
fn script_names_caps_at_four_names() {
    assert_eq!(script_names("a.exe b.sh c.py d.js e.ps1"), "a.exe b.sh c.py d.js");
}

#[test]
fn script_names_empty_input() {
    assert_eq!(script_names(""), "");
}

// ---------- Series::push ----------

#[test]
fn series_push_caps_at_cap() {
    let mut s = Series::new();
    for i in 0..(CAP + 50) {
        s.push(i as f64, 0.0, i as u64);
    }
    assert_eq!(s.v.len(), CAP);
    assert_eq!(s.v.front().unwrap().0, 50.0);
    assert_eq!(s.last_seen, (CAP + 49) as u64);
}

// ---------- Series::trend ----------

#[test]
fn trend_steady_growth_reports_true_rate() {
    // 60 s samples over one hour, value rises 2.0 per hour from 100
    let s = series(&spaced(61, 60.0, |t| 100.0 + 2.0 * t / 3600.0));
    let (slope, up, used, cur) = s.trend(1000).expect("enough data");
    assert!((slope - 2.0).abs() < 1e-6, "slope {slope}");
    assert_eq!(up, 1.0);
    assert_eq!(used, 61);
    assert!((cur - 102.0).abs() < 1e-9);
}

#[test]
fn trend_declining_series_is_negative() {
    let s = series(&spaced(31, 60.0, |t| 50.0 - 4.0 * t / 3600.0));
    let (slope, up, _, _) = s.trend(100).expect("enough data");
    assert!((slope + 4.0).abs() < 1e-6, "slope {slope}");
    assert_eq!(up, 0.0);
}

#[test]
fn trend_flat_series_has_zero_slope() {
    let s = series(&spaced(30, 60.0, |_| 42.0));
    let (slope, _, _, cur) = s.trend(100).expect("enough data");
    assert!(slope.abs() < 1e-9, "slope {slope}");
    assert_eq!(cur, 42.0);
}

#[test]
fn trend_needs_ten_samples() {
    let s = series(&spaced(9, 600.0, |t| t));
    assert!(s.trend(100).is_none());
    // window smaller than 10 also yields None, even with a long history
    let long = series(&spaced(50, 60.0, |t| t));
    assert!(long.trend(9).is_none());
}

#[test]
fn trend_needs_five_minutes_of_span() {
    let s = series(&spaced(20, 10.0, |t| t)); // 190 s span
    assert!(s.trend(100).is_none());
}

#[test]
fn trend_accepts_exactly_five_minutes() {
    let s = series(&spaced(11, 30.0, |t| t)); // 300 s span
    assert!(s.trend(100).is_some());
}

#[test]
fn trend_uses_only_last_n_points() {
    // first 100 samples flat at 10, last 50 rise at 1.0 per hour
    let mut pts = spaced(100, 60.0, |_| 10.0);
    let tail_start = 100.0 * 60.0;
    for i in 0..50 {
        let t = tail_start + i as f64 * 60.0;
        pts.push((t, 10.0 + (t - tail_start) / 3600.0));
    }
    let s = series(&pts);
    let (slope, _, used, _) = s.trend(50).expect("enough data");
    assert_eq!(used, 50);
    assert!((slope - 1.0).abs() < 1e-6, "slope {slope}");
}

// ---------- iso ----------

#[test]
fn iso_formats_known_instants() {
    assert_eq!(iso(0.0), "1970-01-01T00:00:00Z");
    assert_eq!(iso(86399.0), "1970-01-01T23:59:59Z");
    assert_eq!(iso(951782400.0), "2000-02-29T00:00:00Z");
    assert_eq!(iso(1709164800.0), "2024-02-29T00:00:00Z");
    assert_eq!(iso(1700000000.0), "2023-11-14T22:13:20Z");
}

#[test]
fn iso_handles_pre_epoch_and_truncates_fraction() {
    assert_eq!(iso(-1.0), "1969-12-31T23:59:59Z");
    assert_eq!(iso(1.999), "1970-01-01T00:00:01Z");
}

// ---------- tag_hint ----------

#[test]
fn tag_hint_maps_known_tags() {
    assert!(tag_hint("Toke").starts_with("token objects"));
    assert!(tag_hint("FMfn").starts_with("Filter Manager"));
    assert!(tag_hint("WCsc").starts_with("wcifs.sys"));
    assert!(tag_hint("File").starts_with("file objects"));
    assert!(tag_hint("NtfF").starts_with("NTFS metadata"));
    assert!(tag_hint("Proc").starts_with("process/thread/job"));
    assert!(tag_hint("Job ").starts_with("process/thread/job"));
    assert!(tag_hint("MmSt").starts_with("memory-manager"));
}

#[test]
fn tag_hint_unknown_tag_falls_back() {
    assert!(tag_hint("ZZZZ").starts_with("unknown tag"));
    assert!(tag_hint("").starts_with("unknown tag"));
}
/// Live check, no action taken: cargo test --release live_cpu -- --ignored --nocapture
#[test]
#[ignore]
fn live_cpu_over_20s() {
    let mut buf = vec![0u8; 1 << 20];
    let a = read_procs(&mut buf).unwrap_or_default();
    let t0 = std::time::Instant::now();
    std::thread::sleep(std::time::Duration::from_secs(20));
    let b = read_procs(&mut buf).unwrap_or_default();
    let dt = t0.elapsed().as_secs_f64();
    let prev: HashMap<u32, u64> = a.iter().map(|p| (p.pid, p.cpu)).collect();
    let mut rows: Vec<(String, u32, f64)> = Vec::new();
    let mut samples = Vec::new();
    for p in b.iter().filter(|p| p.pid > 4) {
        if let Some(&c0) = prev.get(&p.pid) {
            let d = p.cpu.saturating_sub(c0) as f64 / 1e7;
            rows.push((p.name.clone(), p.pid, d / dt * 100.0));
            samples.push(cpuhog::Sample { pid: p.pid, name: p.name.clone(), created: p.created, delta_s: d, total_s: p.cpu as f64 / 1e7 });
        }
    }
    rows.sort_by(|x, y| y.2.partial_cmp(&x.2).unwrap());
    for r in rows.iter().take(10) { println!("{:>6.1}% of a core  {} (pid {})", r.2, r.0, r.1); }
    let mut h = cpuhog::CpuHogs::new();
    let out = h.update(0.0, dt, &samples, &|_| "?".into());
    println!("detector after 20 s (needs 10 min of history, so none expected): {} alerts", out.len());
}
