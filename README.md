（根目录下运行）使用：

1、需要先迁移数据库（前两次）、设置好.env

python datahandle/data_import/import.py  ：导入数据

2、需要设置好.env中必要的字段

批量下载：`cargo run -p crates_io -- download`

批量构建：`cargo run -p crates_io -- build`

单元测试：

运行 crates_io 包的全部测试：
`cargo test -p crates_io`

1、测试 `.env` 中 `DOWNLOAD_DIR` 路径是否存在：
`cargo test -p crates_io env_download_dir_exists -- --nocapture`

2、测试下载目录分桶规则函数 (目录划分)：
`cargo test -p crates_io test_get_crate_file_path`
