/*把 SeaORM migration的常用类型（ MigratorTrait 、MigrationTrait 、SchemaManager 等）重新导出，方便本crate其他模块直接用。
(SeaORM异步ORM，通过rust结构体和方法操作数据库  
SeaORM migration一种机制，能够像管理代码版本一样管理数据库的结构变化。每个迁移文件描述了一次数据库变更以及如何回滚该变更)*/
pub use sea_orm_migration::prelude::*;

//把每个迁移文件注册成模块，让 Rust 编译时能找到它们。
mod m20260425_000001_create_crates_table;
mod m20260425_000002_add_download_retry_fields;
mod m20260426_000003_add_version_handled_to_crates;
mod m20260426_000004_create_crate_versions_index_table;
mod m20260427_000005_add_compile_fields_to_crates;

//每个文件里都会定义一个 pub struct Migration; 并实现 MigrationTrait ，描述“这次迁移要做什么”
pub struct Migrator;

//migrations()返回一个列表，告诉SeaORM有哪些迁移，按什么顺序跑
#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260425_000001_create_crates_table::Migration),
            Box::new(m20260425_000002_add_download_retry_fields::Migration),
            Box::new(m20260426_000003_add_version_handled_to_crates::Migration),
            Box::new(m20260426_000004_create_crate_versions_index_table::Migration),
            Box::new(m20260427_000005_add_compile_fields_to_crates::Migration),
        ]
    }
}
