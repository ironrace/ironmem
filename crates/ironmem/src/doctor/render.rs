//! Human-readable rendering of a [`DoctorReport`](super::DoctorReport).

use super::{CheckStatus, DoctorReport};

fn marker(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Ok => "[ OK ]",
        CheckStatus::Info => "[INFO]",
        CheckStatus::Warn => "[WARN]",
        CheckStatus::Error => "[FAIL]",
    }
}

/// Render the report as an aligned, human-readable block. Each check is one
/// line; any remediation hint is shown indented beneath it. A trailing summary
/// counts each severity and states the overall verdict.
pub fn render_text(report: &DoctorReport) -> String {
    let mut out = String::from("ironmem doctor\n\n");

    let name_width = report
        .checks
        .iter()
        .map(|c| c.name.len())
        .max()
        .unwrap_or(0);

    let (mut oks, mut infos, mut warns, mut errors) = (0usize, 0usize, 0usize, 0usize);
    for c in &report.checks {
        match c.status {
            CheckStatus::Ok => oks += 1,
            CheckStatus::Info => infos += 1,
            CheckStatus::Warn => warns += 1,
            CheckStatus::Error => errors += 1,
        }
        out.push_str(&format!(
            "{} {:<width$}  {}\n",
            marker(c.status),
            c.name,
            c.summary,
            width = name_width
        ));
        if let Some(hint) = &c.hint {
            out.push_str(&format!(
                "       {:<width$}  → {hint}\n",
                "",
                width = name_width
            ));
        }
    }

    out.push('\n');
    let verdict = if report.has_blocking() {
        "blocking setup failures found"
    } else {
        "no blocking failures"
    };
    out.push_str(&format!(
        "Summary: {oks} ok, {infos} info, {warns} warning(s), {errors} error(s) — {verdict}\n"
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::super::Check;
    use super::*;

    fn sample() -> DoctorReport {
        DoctorReport {
            checks: vec![
                Check::new("binary", CheckStatus::Ok, "ironmem 1.0.0"),
                Check {
                    name: "model",
                    status: CheckStatus::Error,
                    summary: "model not found".into(),
                    hint: Some("run `ironmem setup`".into()),
                },
            ],
        }
    }

    #[test]
    fn render_includes_markers_summaries_and_hints() {
        let text = render_text(&sample());
        assert!(text.contains("[ OK ]"));
        assert!(text.contains("[FAIL]"));
        assert!(text.contains("ironmem 1.0.0"));
        assert!(text.contains("run `ironmem setup`"));
    }

    #[test]
    fn render_summary_reports_blocking_verdict() {
        let text = render_text(&sample());
        assert!(text.contains("1 error(s)"));
        assert!(text.contains("blocking setup failures found"));
    }

    #[test]
    fn render_summary_reports_clean_verdict() {
        let report = DoctorReport {
            checks: vec![Check::new("binary", CheckStatus::Ok, "ironmem 1.0.0")],
        };
        let text = render_text(&report);
        assert!(text.contains("no blocking failures"));
    }
}
