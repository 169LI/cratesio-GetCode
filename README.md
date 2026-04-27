（根目录下运行）使用：

1、需要先迁移数据库、导入预处理数据

数据库迁移：见datahandle/migrations/src/main.rs具体说明

基础数据的导入： `cargo run -p crates_io -- data-batch import-base`

版本号、依赖关系的解析与导入(批量下载后可运行)：`cargo run -p crates_io -- data-batch handle-version`

2、需要设置好.env中必要的字段

批量下载：`cargo run -p crates_io -- download`

批量构建：`cargo run -p crates_io -- build`

单元测试：

运行 crates_io 包的全部测试：
`cargo test -p crates_io`