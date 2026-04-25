
pub use sea_orm_migration::prelude::*;

mod m20260425_000001_create_crates_table;
mod m20260425_000002_add_download_retry_fields;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260425_000001_create_crates_table::Migration),
            Box::new(m20260425_000002_add_download_retry_fields::Migration),
        ]
    }
}
