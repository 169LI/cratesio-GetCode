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
                        ColumnDef::new(Crates::CompileHandled)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .add_column(
                        ColumnDef::new(Crates::InitialCompileFailed)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .add_column(
                        ColumnDef::new(Crates::CargoLockExists)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .add_column(ColumnDef::new(Crates::DepUpdateErrors).json_binary())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Crates::Table)
                    .drop_column(Crates::CompileHandled)
                    .drop_column(Crates::InitialCompileFailed)
                    .drop_column(Crates::CargoLockExists)
                    .drop_column(Crates::DepUpdateErrors)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum Crates {
    Table,
    CompileHandled,
    InitialCompileFailed,
    CargoLockExists,
    DepUpdateErrors,
}
