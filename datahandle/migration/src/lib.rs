pub use sea_orm_migration::prelude::*;

mod m20260425_000001_create_crates_table;
mod m20260425_000002_add_download_retry_fields;
mod m20260426_000003_add_version_handled_to_crates;
mod m20260426_000004_create_crate_versions_index_table;
mod m20260427_000005_add_compile_fields_to_crates;
mod m20260512_000006_add_heavy_deps_skip_fields_to_crates;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260425_000001_create_crates_table::Migration),
            Box::new(m20260425_000002_add_download_retry_fields::Migration),
            Box::new(m20260426_000003_add_version_handled_to_crates::Migration),
            Box::new(m20260426_000004_create_crate_versions_index_table::Migration),
            Box::new(m20260427_000005_add_compile_fields_to_crates::Migration),
            Box::new(m20260512_000006_add_heavy_deps_skip_fields_to_crates::Migration),
        ]
    }
}
