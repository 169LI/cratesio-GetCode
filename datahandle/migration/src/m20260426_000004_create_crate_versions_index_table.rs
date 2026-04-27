use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(CrateVersionsIndex::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(CrateVersionsIndex::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(CrateVersionsIndex::CrateId)
                            .integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(CrateVersionsIndex::Version)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(CrateVersionsIndex::Deps)
                            .json_binary()
                            .not_null(),
                    )
                    .col(ColumnDef::new(CrateVersionsIndex::Features2).json_binary())
                    .col(ColumnDef::new(CrateVersionsIndex::Pubtime).timestamp())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_crate_versions_index_crate_id")
                            .from(CrateVersionsIndex::Table, CrateVersionsIndex::CrateId)
                            .to(Crates::Table, Crates::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .index(
                        Index::create()
                            .name("idx_crate_versions_index_crate_version_unique")
                            .table(CrateVersionsIndex::Table)
                            .col(CrateVersionsIndex::CrateId)
                            .col(CrateVersionsIndex::Version)
                            .unique(),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(CrateVersionsIndex::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum CrateVersionsIndex {
    Table,
    Id,
    CrateId,
    Version,
    Deps,
    Features2,
    Pubtime,
}

#[derive(DeriveIden)]
enum Crates {
    Table,
    Id,
}
