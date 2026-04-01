use minijinja::Environment;

pub fn build_env() -> Environment<'static> {
    let mut env = Environment::new();

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
