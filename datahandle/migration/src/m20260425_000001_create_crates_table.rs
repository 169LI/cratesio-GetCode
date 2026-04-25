use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Crates::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Crates::Id)
                            .integer()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Crates::Name).string().not_null())
                    .col(ColumnDef::new(Crates::Homepage).string())
                    .col(
                        ColumnDef::new(Crates::Analyzed)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .col(
                        ColumnDef::new(Crates::Download)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .col(
                        ColumnDef::new(Crates::CreatedAt)
                            .timestamp()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .col(
                        ColumnDef::new(Crates::UpdatedAt)
                            .timestamp()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .col(
                        ColumnDef::new(Crates::VersionNew)
                            .string()
                            .not_null()
                            .default(""),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Crates::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum Crates {
    Table,
    Id,
    Name,
    Homepage,
    Analyzed,
    Download,
    CreatedAt,
    UpdatedAt,
    VersionNew,
}
