pub mod entities;

use dotenvy::dotenv;
use sea_orm::{Database, DatabaseConnection};

pub fn load_env() {
    dotenv().ok();
}

pub async fn connect_from_env() -> Result<DatabaseConnection, sea_orm::DbErr> {
    load_env();
    let database_url = std::env::var("DATABASE_URL")
        .map_err(|_| sea_orm::DbErr::Custom("DATABASE_URL not set".into()))?;
    Database::connect(database_url).await
}
