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

pub fn build_env() -> Environment<'static> {
    let mut env = Environment::new();

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

    env
}
