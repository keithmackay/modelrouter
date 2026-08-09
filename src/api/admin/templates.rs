use minijinja::Environment;
use std::sync::OnceLock;

static ENV: OnceLock<Environment<'static>> = OnceLock::new();

pub fn env() -> &'static Environment<'static> {
    ENV.get_or_init(build_env)
}

/// Format an RFC3339 timestamp as "YYYY-MM-DD HH:MM:SS" for display.
pub fn fmt_ts(s: &str) -> String {
    let trimmed = if s.len() >= 19 { &s[..19] } else { s };
    trimmed.replace('T', " ")
}

/// Format a USD amount so small spends stay legible.
///
/// Per-request LLM costs are routinely fractions of a cent, and a plain
/// `round(2)` renders every one of them as "$0.00" — the dashboard showed a
/// column of zeroes whose total was clearly not zero. Scale the unit to the
/// magnitude instead:
///
/// | amount            | rendered   |
/// |-------------------|------------|
/// | >= $1             | `$12.34`   |
/// | >= 1 cent         | `$0.1217`  |
/// | > 0, < 1 cent     | `0.0044c`  |
/// | exactly 0         | `$0.00`    |
///
/// Sub-cent values are shown in cents because "0.0044c" carries the magnitude
/// at a glance where "$0.000044" is a digit-counting exercise.
pub fn fmt_money(amount: f64) -> String {
    if !amount.is_finite() {
        return "-".to_string();
    }
    let a = amount.abs();
    let sign = if amount < 0.0 { "-" } else { "" };
    if a == 0.0 {
        "$0.00".to_string()
    } else if a >= 1.0 {
        format!("{}${:.2}", sign, a)
    } else if a >= 0.01 {
        format!("{}${:.4}", sign, a)
    } else {
        format!("{}{:.4}c", sign, a * 100.0)
    }
}

pub fn build_env() -> Environment<'static> {
    let mut env = Environment::new();

    env.add_filter("money", |v: minijinja::Value| -> minijinja::Value {
        let amount: f64 = f64::try_from(v.clone()).unwrap_or_else(|_| v.to_string().parse().unwrap_or(0.0));
        minijinja::Value::from(fmt_money(amount))
    });

    env.add_filter("fmt_ts", |v: minijinja::Value| -> minijinja::Value {
        let s = v.to_string();
        let trimmed = if s.len() >= 19 { &s[..19] } else { &s };
        minijinja::Value::from(trimmed.replace('T', " "))
    });

    env.add_template_owned(
        "base.html",
        include_str!("../../../templates/admin/base.html").to_string(),
    )
    .expect("base.html template is valid");

    env.add_template_owned(
        "login.html",
        include_str!("../../../templates/admin/login.html").to_string(),
    )
    .expect("login.html template is valid");

    env.add_template_owned(
        "attribution_panels.html",
        include_str!("../../../templates/admin/attribution_panels.html").to_string(),
    )
    .expect("attribution_panels.html template is valid");

    env.add_template_owned(
        "cache.html",
        include_str!("../../../templates/admin/cache.html").to_string(),
    )
    .expect("cache.html template is valid");

    env.add_template_owned(
        "overview.html",
        include_str!("../../../templates/admin/overview.html").to_string(),
    )
    .expect("overview.html template is valid");

    env.add_template_owned(
        "keys.html",
        include_str!("../../../templates/admin/keys.html").to_string(),
    )
    .expect("keys.html template is valid");

    env.add_template_owned(
        "users.html",
        include_str!("../../../templates/admin/users.html").to_string(),
    )
    .expect("users.html template is valid");

    env.add_template_owned(
        "prompts.html",
        include_str!("../../../templates/admin/prompts.html").to_string(),
    )
    .expect("prompts.html template is valid");

    env.add_template_owned(
        "cost.html",
        include_str!("../../../templates/admin/cost.html").to_string(),
    )
    .expect("cost.html template is valid");

    env.add_template_owned(
        "hooks.html",
        include_str!("../../../templates/admin/hooks.html").to_string(),
    )
    .expect("hooks.html template is valid");

    env.add_template_owned(
        "audit.html",
        include_str!("../../../templates/admin/audit.html").to_string(),
    )
    .expect("audit.html template is valid");

    env.add_template_owned(
        "admins.html",
        include_str!("../../../templates/admin/admins.html").to_string(),
    )
    .expect("admins.html template is valid");

    env.add_template_owned(
        "groups.html",
        include_str!("../../../templates/admin/groups.html").to_string(),
    )
    .expect("groups.html template is valid");

    env.add_template_owned(
        "budgets.html",
        include_str!("../../../templates/admin/budgets.html").to_string(),
    )
    .expect("budgets.html template is valid");

    env.add_template_owned(
        "reports.html",
        include_str!("../../../templates/admin/reports.html").to_string(),
    )
    .expect("reports.html template is valid");

    env.add_template_owned(
        "reports_panels.html",
        include_str!("../../../templates/admin/reports_panels.html").to_string(),
    )
    .expect("reports_panels.html template is valid");

    env.add_template_owned(
        "models.html",
        include_str!("../../../templates/admin/models.html").to_string(),
    )
    .expect("models.html template is valid");

    env
}


#[cfg(test)]
mod money_tests {
    use super::fmt_money;

    #[test]
    fn scales_the_unit_to_the_magnitude() {
        assert_eq!(fmt_money(0.0), "$0.00");
        assert_eq!(fmt_money(12.3456), "$12.35");
        assert_eq!(fmt_money(0.1217), "$0.1217");
        // The case that motivated this: round(2) rendered these as "$0.00".
        assert_eq!(fmt_money(0.000044), "0.0044c");
        assert_eq!(fmt_money(0.00004365), "0.0044c");
    }

    #[test]
    fn handles_negative_and_non_finite() {
        assert_eq!(fmt_money(-2.5), "-$2.50");
        assert_eq!(fmt_money(-0.00001), "-0.0010c");
        assert_eq!(fmt_money(f64::NAN), "-");
    }
}
