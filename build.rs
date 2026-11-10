// `sqlx::migrate!` embeds the migration files at compile time; without this a
// newly added migration is silently left out until something else triggers a
// rebuild.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
