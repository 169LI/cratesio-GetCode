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

use datahandle::entities::{crate_versions_index, crates};
use sea_orm::ActiveValue::Set;
use sea_orm::DbErr;
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
    pub version_handled: bool,
}

#[derive(Debug, Clone)]
pub struct CrateVersionIndexRow {
    pub crate_id: i32,
    pub version: String,
    pub deps: sea_orm::prelude::Json,
    pub features2: Option<sea_orm::prelude::Json>,
    pub pubtime: Option<sea_orm::prelude::DateTime>,
}

#[derive(Clone, Debug)]
pub struct PgDataHandle {
    pub connection: Arc<DatabaseConnection>,
}

impl PgDataHandle {
    /// 建立数据库连接，并返回可克隆的句柄（内部通过 Arc 共享连接）。
    ///
    /// 主要用于 CLI 启动阶段初始化数据库连接池。
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

    /// 访问底层 SeaORM 的数据库连接（连接池）。
    ///
    /// 仅用于少数需要直接调用 SeaORM API 的场景，正常情况下尽量复用本模块提供的方法。
    pub fn get_connection(&self) -> &DatabaseConnection {
        &self.connection
    }

    /// 获取需要下载源码的 crate 列表
    ///
    /// 用于下载任务：
    /// - 仅挑选尚未下载（download=false）
    /// - 且未标记为下载失败（download_failed=false）
    /// - 且有可用版本信息（version_new 非空且非 yanked）
    ///
    /// 返回的每条记录对应 `crates` 表的一行。
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

    /// 获取尚未处理版本索引数据的 crate 列表（一次性取全量）。
    ///
    /// 用于版本索引导入任务：
    /// - 仅挑选 version_handled=false（还没被“尝试处理过”）
    /// - 且 download_failed=false（避免对下载失败的 crate 做后续处理）
    /// - 且 name 非空
    ///
    /// 注意：这是全量查询，数据量大时会占用较多内存。
    pub async fn get_all_unhandled_version_crates(
        &self,
    ) -> Result<Vec<crates::Model>, sea_orm::DbErr> {
        crates::Entity::find()
            .filter(crates::Column::VersionHandled.eq(false))
            .filter(crates::Column::DownloadFailed.eq(false))
            .filter(crates::Column::Name.ne(""))
            .all(self.get_connection())
            .await
    }

    /// 标记 crate 下载成功
    ///
    /// 用于下载任务成功后更新 `crates` 表：
    /// - download=true
    /// - download_failed=false（清理此前可能写入的失败标记）
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

    /// 标记 crate 下载失败
    ///
    /// 用于下载任务连续失败后更新 `crates` 表：
    /// - download_failed=true
    ///
    /// 目的：避免下一轮批处理不断重复卡住同一条记录。
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

    /// 标记 crate 的版本索引已经“处理过/尝试过”
    ///
    /// 用于版本索引导入任务每个 crate 处理完成后更新 `crates` 表：
    /// - version_handled=true
    ///
    /// 当前约定：无论该 crate 的版本索引导入成功还是失败，都应当设置为 true，
    /// 代表“已经处理过（不会再重复尝试）”，失败原因通过日志追踪。
    pub async fn mark_crate_version_handled(&self, id: i32) -> Result<(), sea_orm::DbErr> {
        crates::Entity::update_many()
            .col_expr(
                crates::Column::VersionHandled,
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

        // 批量写入 crates 基础数据。
        //
        // 行为：
        // - insert_many 批量插入
        // - 主键冲突（id）时更新部分字段（相当于 UPSERT）
        //
        // 返回值：尝试写入的行数（rows.len），不区分插入/更新。
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
            version_handled: Set(r.version_handled),
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

    /// 批量写入 crate_versions_index（按 crate_id + version 做 UPSERT）。
    ///
    /// 行为：
    /// - insert_many 批量插入版本索引行
    /// - 唯一键冲突（crate_id, version）时更新 deps/features2/pubtime
    ///
    /// 返回值：尝试写入的行数（rows.len），不区分插入/更新。
    pub async fn upsert_crate_versions_index_rows(
        &self,
        rows: Vec<CrateVersionIndexRow>,
    ) -> Result<u64, DbErr> {
        let rows_len: u64 = rows.len().try_into().unwrap_or(u64::MAX);
        if rows_len == 0 {
            return Ok(0);
        }

        let active_models = rows.into_iter().map(|r| crate_versions_index::ActiveModel {
            crate_id: Set(r.crate_id),
            version: Set(r.version),
            deps: Set(r.deps),
            features2: Set(r.features2),
            pubtime: Set(r.pubtime),
            ..Default::default()
        });

        let res = crate_versions_index::Entity::insert_many(active_models)
            .on_conflict(
                OnConflict::columns([
                    crate_versions_index::Column::CrateId,
                    crate_versions_index::Column::Version,
                ])
                .update_columns([
                    crate_versions_index::Column::Deps,
                    crate_versions_index::Column::Features2,
                    crate_versions_index::Column::Pubtime,
                ])
                .to_owned(),
            )
            .exec(self.get_connection())
            .await?;

        let _ = res;
        Ok(rows_len)
    }
}
