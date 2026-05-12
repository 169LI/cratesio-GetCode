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
                        ColumnDef::new(Crates::HeavyDepsSkipped)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .add_column(
                        ColumnDef::new(Crates::HeavyDepsCount)
                            .integer()
                            .not_null()
                            .default(0),
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
                    .drop_column(Crates::HeavyDepsSkipped)
                    .drop_column(Crates::HeavyDepsCount)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum Crates {
    Table,
    HeavyDepsSkipped,
    HeavyDepsCount,
}
