//! 模块说明
//! -
//! Postgres（SeaORM）数据库连接句柄封装。
//!
//! 设计目标
//! -
//! - CLI 入口只负责初始化配置与连接，本模块负责提供可复用的连接手柄 `PgDataHandle`
//! - 具体的读写/查询方法后续再补充（由各业务模块调用 `get_connection()` 自行实现）
//!
//! 依赖
//! -
//! - 连接由 `sea_orm::Database::connect(ConnectOptions)` 建立
//! - 连接串从上层配置读取（例如 `DATABASE_URL`）

use std::sync::Arc;
use std::time::Duration;

use datahandle::entities::crates;
use sea_orm::ActiveValue::Set;
use sea_orm::Condition;
use sea_orm::DbErr;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ColumnTrait, ConnectOptions, Database, DatabaseConnection, EntityTrait, QueryFilter,
    QuerySelect,
};

#[derive(Debug, Clone)]
pub struct CrateImportRow {
    pub id: i32,
    pub name: String,
    pub homepage: Option<String>,
    pub analyzed: bool,
    pub download: bool,
    pub created_at: sea_orm::prelude::DateTime,
    pub updated_at: sea_orm::prelude::DateTime,
    pub version_new: String,
    pub download_failed: bool,
}

#[derive(Clone, Debug)]
pub struct PgDataHandle {
    pub connection: Arc<DatabaseConnection>,
}

impl PgDataHandle {
    pub async fn new(database_url: &str) -> Result<Self, sea_orm::DbErr> {
        let mut opt = ConnectOptions::new(database_url.to_owned());
        opt.max_connections(50)
            .min_connections(1)
            .connect_timeout(Duration::from_secs(8))
            .acquire_timeout(Duration::from_secs(8))
            .idle_timeout(Duration::from_secs(30))
            .max_lifetime(Duration::from_secs(60))
            .sqlx_logging(false);

        let connection = Database::connect(opt).await?;
        Ok(Self {
            connection: Arc::new(connection),
        })
    }

    pub fn get_connection(&self) -> &DatabaseConnection {
        &self.connection
    }

    /// 获取需要下载源码的 crate 列表
    pub async fn get_unfetched_crates(
        &self,
        limit: u64,
    ) -> Result<Vec<crates::Model>, sea_orm::DbErr> {
        crates::Entity::find()
            .filter(crates::Column::Download.eq(false))
            .filter(crates::Column::DownloadFailed.eq(false))
            .filter(crates::Column::VersionNew.is_not_null())
            .filter(crates::Column::VersionNew.ne("yanked"))
            .filter(crates::Column::VersionNew.ne(""))
            .limit(limit)
            .all(self.get_connection())
            .await
    }

    /// 标记 crate 下载成功
    pub async fn mark_crate_downloaded(&self, id: i32) -> Result<(), sea_orm::DbErr> {
        crates::Entity::update_many()
            .col_expr(
                crates::Column::Download,
                sea_orm::sea_query::Expr::value(true),
            )
            .col_expr(
                crates::Column::DownloadFailed,
                sea_orm::sea_query::Expr::value(false),
            )
            .filter(crates::Column::Id.eq(id))
            .exec(self.get_connection())
            .await?;
        Ok(())
    }

    pub async fn mark_crate_download_failed(&self, id: i32) -> Result<(), sea_orm::DbErr> {
        crates::Entity::update_many()
            .col_expr(
                crates::Column::DownloadFailed,
                sea_orm::sea_query::Expr::value(true),
            )
            .filter(crates::Column::Id.eq(id))
            .exec(self.get_connection())
            .await?;
        Ok(())
    }

    pub async fn upsert_crates_import_rows(&self, rows: Vec<CrateImportRow>) -> Result<u64, DbErr> {
        let rows_len: u64 = rows.len().try_into().unwrap_or(u64::MAX);
        if rows_len == 0 {
            return Ok(0);
        }

        let active_models = rows.into_iter().map(|r| crates::ActiveModel {
            id: Set(r.id),
            name: Set(r.name),
            homepage: Set(r.homepage),
            analyzed: Set(r.analyzed),
            download: Set(r.download),
            created_at: Set(r.created_at),
            updated_at: Set(r.updated_at),
            version_new: Set(r.version_new),
            download_failed: Set(r.download_failed),
        });

        let res = crates::Entity::insert_many(active_models)
            .on_conflict(
                OnConflict::column(crates::Column::Id)
                    .update_columns([
                        crates::Column::Name,
                        crates::Column::Homepage,
                        crates::Column::Analyzed,
                        crates::Column::Download,
                        crates::Column::CreatedAt,
                        crates::Column::UpdatedAt,
                        crates::Column::VersionNew,
                        crates::Column::DownloadFailed,
                    ])
                    .to_owned(),
            )
            .exec(self.get_connection())
            .await?;

        let _ = res;
        Ok(rows_len)
    }

    pub async fn mark_yanked_for_empty_crate_version(&self) -> Result<u64, DbErr> {
        let condition = Condition::any()
            .add(crates::Column::VersionNew.is_null())
            .add(crates::Column::VersionNew.eq(""));

        let res = crates::Entity::update_many()
            .col_expr(crates::Column::Download, Expr::value(true))
            .col_expr(crates::Column::VersionNew, Expr::value("yanked"))
            .filter(condition)
            .exec(self.get_connection())
            .await?;

        Ok(res.rows_affected)
    }
}
