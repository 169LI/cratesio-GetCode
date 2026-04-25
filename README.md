（根目录下运行）使用：


1、需要先迁移数据库（前两次）、设置好.env

python datahandle/data_import/import.py  ：导入数据

2、需要设置好.env中必要的字段

批量下载：`cargo run -p crates_io -- download`

批量构建：`cargo run -p crates_io -- build`
