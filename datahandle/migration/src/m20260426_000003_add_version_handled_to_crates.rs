use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Crates::Table)
                    .add_column(
                        ColumnDef::new(Crates::VersionHandled)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Crates::Table)
                    .drop_column(Crates::VersionHandled)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum Crates {
    Table,
    VersionHandled,
}
