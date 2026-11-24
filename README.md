# jackcess-rs

[![CI](https://github.com/pcmaclk/jackcess-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/pcmaclk/jackcess-rs/actions/workflows/ci.yml)

[English](README.en.md) | 简体中文

[Jackcess](https://jackcess.sourceforge.io/) 5.0.4 的 Rust 移植。Jackcess 是一个纯 Java
库，用于读写 Microsoft Access 数据库（Jet / ACE 的 `.mdb` 与 `.accdb` 文件）。

本移植力求忠实：对于同样的操作，Rust 代码写出的文件与原 Java 库写出的文件
**逐字节完全一致（byte-for-byte identical）**。一套自动化 harness 会用同一场景分别运行
Java 参考生成器与 Rust 生成器，并比较生成的文件。

## 从克隆的仓库构建

```
cargo build
cargo test --lib        # 327 tests; no test fixtures needed
```

本仓库是**可构建的最小 crate**：只包含源代码，以及库在编译期嵌入的 zlib 压缩资源（约 1.5 MB）
（`resources/z/` 下的模板、索引排序表和生成的代码页表，首次使用时解压）。验证 harness、Java 参考工具
以及 Java 与 Rust 对照用的测试 fixture 只用于开发，**不属于**本仓库。

依赖 fixture 的测试由 `fixtures` feature 控制，因此只有在带有测试数据的开发树中才会
编译并运行：

```
cargo test --lib --features fixtures
```

开发用的 harness 还会使用 Java 参考库和真实的 Access/加密语料。`fetch-external.ps1`
负责下载这些内容（它不会恢复开发用的测试 fixture）：

```
powershell -ExecutionPolicy Bypass -File fetch-external.ps1
# behind a local proxy:
powershell -ExecutionPolicy Bypass -File fetch-external.ps1 -Proxy http://proxy.example:8080
```

## Feature flags

本 crate 有两个可关闭的 feature；二者默认开启，因此直接 `cargo build` 得到的就是完整、
逐字节一致的库：

| feature | 增加的内容 | 大致的二进制体积开销 |
| --- | --- | --- |
| `create` | 内嵌的 `empty*.mdb/accdb` 模板与 `Database::create*` | 约 0.1 MB（压缩后的模板） |
| `crypto` | 页编码、Jet/Office 密码、兼容 jackcess-encrypt 的 handler、MSISAM（`.mny`）解码，以及加密相关依赖 | 在 `mdbgen.exe` 中约 1.9 MB |

只读应用如果只打开未编码的 Jet/ACE 数据库，可以同时关闭二者：

```toml
[dependencies]
jackcess = { version = "5.0.4-rs.1", default-features = false }
```

使用 `--no-default-features` 时，创建数据库会返回 `Error::Unsupported`（除非启用
`create`）；打开编码过的文件或 MSISAM 文件会返回 `Unsupported`（除非启用 `crypto`）；
普通的未加密数据库可正常打开。以下是使用 `lto = "fat"`、`codegen-units = 1`、
`panic = "abort"` 和 `strip = "symbols"` 实测的结果：

| 构建 | 大小 |
| --- | --- |
| 最小消费者（打开数据库 + 读取表名），默认 features | 3.63 MB |
| 最小消费者，`default-features = false` | 3.37 MB |
| `mdbgen.exe`，默认 features | 5.62 MB |
| `mdbgen.exe`，`--no-default-features` | 5.30 MB |

`query`、`export`、`ole`、`complex`、`deflate` 和 `expr` 不是独立的 feature：表的读写
路径会直接用到复杂列、OLE blob、计算列和表达式校验，而 query/export 相对于这些核心
依赖来说很小。它们在任何配置下都会参与编译。

## 版本号

版本号由两个独立部分组成：

```
<jackcess version>-rs.<port revision>
       5.0.4        -rs.1
```

* **`<jackcess 版本>`** 是本移植所针对的上游 Jackcess 版本，决定文件格式、API 和字节级
  行为。迁移到更新的上游版本（`5.0.5`、`5.1.0`、……）会开启新的 `<jackcess 版本>` 段。
* **`<移植修订号>`** 统计该基线下的 Rust 侧发布：bug 修复、API 兼容性改动、依赖/安全
  更新，或任何其他对发布代码的改动，都会使其递增（`-rs.1`、`-rs.2`、`-rs.3`、……）。
  上游段变化时，它会重置为 `-rs.1`。

规则：

1. **首次发布：** `5.0.4-rs.1`，git tag `v5.0.4-rs.1`。
2. **移植侧 bug 修复 / API 改动：** `5.0.4-rs.2`、`5.0.4-rs.3`、……；tag 为
   `v5.0.4-rs.2`、……。上游段保持 `5.0.4`，这样使用者能看出文件格式/API 基线没有变化。
3. **上游移植版本升级：** `5.0.5-rs.1`，tag `v5.0.5-rs.1`；修订号重新计数。
4. **永远不要复用版本号或 tag。** crates.io 的版本不可变；每一次已发布的改动都使用新的
   `-rs.N`，并且 Git tag 始终等于 crate 版本前加 `v`。
5. **SemVer 语义：** `-rs.N` 是预发布标识，因此普通的 `jackcess = "5.0.4"` 依赖要求
   **不会**选中它。使用者应显式固定本移植的版本，例如 `jackcess = "=5.0.4-rs.2"`，或
   使用 `5.0.4-rs.2` 这样的要求，以允许同一上游基线后续的 `-rs.N` 修订。这里刻意不发布
   普通的 `5.0.4`，以便移植修订号始终可见。
6. **发布说明** 列出上游基线、移植修订号，以及相对于上一个 `-rs.N` 的改动；
   `CHANGELOG.md` 是唯一事实来源。

如果将来确实需要一条稳定的、非预发布的产品线，那是刻意的策略变更：发布新的上游段版本
（例如 `5.0.5`），而不是悄悄去掉现有 `5.0.4` 发布上的后缀。

## 已验证的逐字节一致输出

> 本节记录的 41 个 harness、`tools/`、`java-src/` 与 `external/` 都属于开发树，不随仓库发布；这里保留发布前的验证结果作为记录。仓库内只保留 `src/bin` 下的生成器源码，并且由 `dev-bins` feature 控制。


`tools\verify.ps1`（除非另有说明，否则为 V2003 / Jet 4）：

| 场景 | 作用 | 结果 |
| --- | --- | --- |
| `empty` | 从内嵌模板创建新数据库 | 一致（204,800 字节） |
| `t1` | 创建带主键索引的 `Test(ID autonumber, Name text(50), Value double)` | 一致（217,088） |
| `rows` | `t1` + 5 行 | 一致（225,280） |
| `rows50` | `t1` + 50 行（强制索引页分裂、嵌套索引页和多个数据页） | 一致（225,280） |
| `types` | 创建包含 14 个标量类型列的表 | 一致（212,992） |
| `guidx` | GUID 自动编号主键加一个 GUID 列，包含显式的带花括号/不带花括号/大写/小写值、一个原始字节 GUID，以及普通 GUID 列上的 null | 一致（V2003 225,280；V2007 348,160；V2010 364,544；V2016 385,024） |
| `bigindex` | 逐行插入 3000 行，带自动编号主键、唯一 TEXT 索引、降序复合索引和非唯一变长文本索引（深 B 树、多次分裂、前缀压缩） | 一致（V2003 983,040；V2007 1,105,920；V2010 1,212,416；V2016 1,232,896） |
| `indexchurn` | `bigindex` 表经过 300 次索引键修改、约 215 次非键更新、500 次删除和 500 次重新插入 | 一致（827,392 / 950,272 / 1,019,904 / 1,040,384） |
| `bigindex2` | 2000 行，带 3 列复合索引、两个降序索引、全 null 键和 120 字符共享文本前缀 | 一致（1,875,968 / 1,998,848 / 2,514,944 / 2,535,424） |
| `types_rows` | `types` + 4 行（Unicode 文本、money、numeric、guid、binary、日期、memo、布尔值、一行全 null） | 一致（221,184） |
| `twoindexes` | 带主键、唯一文本索引和降序多列索引的表 + 20 行 | 一致（233,472） |
| `twotables` | 依次创建两个表（目录时间戳不同） | 一致（229,376） |
| `append` | 用 Java 和 Rust 分别打开同一个由 jackcess 创建的数据库，各追加相同的 3 行（包括写入长值页的长 MEMO 值），再比较 | 一致（229,376） |
| `unicode` | 在 TEXT 列上建主键，9 行的键包含重音字符/CJK/emoji（端到端检验文本排序） | 一致（225,280） |
| `bigmemo` | 带 MEMO 列的表，行长度为 10…12,000 个字符（长值存放在其他页，并跨链式页） | 一致（274,432） |
| `delete` | 带 MEMO 列的表 + 150 行，删除 6 行（其中一行持有长 MEMO 值），再添加 3 行；全部在同一次打开文件的过程中完成 | 一致（417,792） |
| `update` | 带 text/double/long/boolean/date 列的表 + 150 行，就地更新 7 行，并通过重定位更新 7 行（更长/更短文本，修改数值/日期/布尔/null 值） | 一致（233,472） |
| `memoupdate` | MEMO 列 + 60 行，把值从行内更新到其他页、从其他页/多页更新回行内、保持不变、置 null，以及跨多个长值页更新，然后添加 2 行 | 一致（294,912） |
| `partialupdate` | `Test(ID autonumber pk, Name text(255) unique, Memo memo, N long)` 带 5 行（短行内 memo、多页 memo）+ 60 行填充；随后做部分行更新，对 `ID`/`Memo` 传入 `Column.KEEP_VALUE`/`Value::Keep`，修改 `Name`/`N`（一次全 keep 更新、一次通过 `updateValue`、一次通过 `asUpdateRow`/`update_row_from_map`、一次 `Name` 增长导致行重定位到另一个数据页而多页 memo 保留其页，以及一次更新 memo 本身并写入新的长值页），随后再添加 2 行 | 一致（266,240） |
| `diag200` | 与 `partialupdate` 相同的表结构，逐行添加 200 行 `filler%03d`（逐行 `addRowFromMap` 路径，每行之后都会写入/分裂索引页，并越过唯一文本索引页的前缀压缩/分裂阈值） | 一致（V2003 245,760；V2007 368,640；V2016 405,504） |
| `keeplong` | 一个多页 MEMO，在第一行其他列经历七次更新、第二行经历两次更新时始终保持不变（从不分配新的长值页），包括 `updateValue` 和 `asUpdateRow`，然后添加一行 | 一致（249,856） |
| `rowops` | 带两个索引（主键 + 唯一文本索引）的表和一个无索引表：两次更新行（先就地重定位，再增长），删除一个已重定位的行，从无索引表删除，然后添加行 | 一致（249,856） |
| `addcol` | `T` + 5 行，然后添加一个 LONG 列（`ColumnBuilder.addToTable`）和 3 行（两行有值，一行 null） | 一致（225,280） |
| `addindex` | 无索引表 + 30 行，然后添加唯一文本索引、共享同一底层索引数据的第二个唯一索引、降序数值索引和主键（`IndexBuilder.addToTable`），再添加 2 行 | 一致（233,472） |
| `addboth` | 添加一个 LONG 列并为它建索引，添加一个 MEMO 列（及其 usage map）和 4 行，全部在同一次打开文件的过程中完成 | 一致（233,472） |
| `addmany` | 添加 120 个 LONG 列，使表定义增长到第二页（更新路径中的定义分裂与页复用） | 一致（229,376） |
| `relate` | `P(ID autonumber pk, Name text(40))` 和 `C(ID autonumber pk, P_ID long, Note text(50))` 带数据行，然后建立带引用完整性的关系 `P.ID → C.P_ID`（在 `P` 上创建 `.rB` 唯一外键索引，复用主键索引数据；在 `C` 上创建 `PC` 外键索引，使用新的索引数据），然后添加更多行 | 一致（258,048） |
| `samerel` | 一次打开中建立两个关系：带级联更新和删除的 `P.ID → C.P_ID`，以及一对一的 `N.ID → D.N_ID`（两侧都有唯一索引，因此一对一标志由推导得出，两个外键索引都复用已有索引数据） | 一致（303,104） |
| `selfrel` | 表与自身关联（`E.ID → E.Parent_ID`）：两个支撑索引都在 `E` 上，索引号相差 1，并互相引用 | 一致（237,568） |
| `dbprops` | 在创建数据库时应用数据库属性（`Title` TEXT、`CustomNum` LONG）、一个摘要属性（`Title`）和一个用户定义属性（`MyBool` BOOLEAN）（`DatabaseBuilder.putDatabaseProperty` 等；Java builder 用 `HashMap` 收集每组属性，因此 blob 顺序也被复现） | 一致（204,800） |
| `tableprops` | 带表 `Description`、一个大到跨越多个长值页的 `RowSource` MEMO 属性、列属性（`Name` 上的 `Description`/`Format`、`Value` 上的 `DefaultValue`、`Note` 上的 `Description`）、3 行，然后修改一个表属性、删除一个列属性、修改一个列属性并再次保存 | 一致（258,048） |
| `propupdate` | 带数据行的普通表，关闭再重新打开后设置表和列的 `Description`（保存到现有 catalog 行），再次重新打开后修改/删除它们并保存 | 一致（225,280） |
| `summaryprops` | 在同一次打开中修改/删除现有数据库的数据库、摘要和用户定义属性组（`MSysDb`、`SummaryInfo` 和 `UserDefined` 行） | 一致（204,800） |
| `linked` | `P`/`C` 加一个关系（占用第一个合成对象 id），然后创建两个链接表（`Database.createLinkedTable`：类型 6 的 catalog 行，带 `Database`/`ForeignName`、ACE，以及接在关系之后的合成 id）和一个普通表 | 一致（249,856） |
| `reclaim` | 带索引的表有 905 行（主键索引增长到多个叶子页），通过主键删除 895 行，使空的索引页被释放，然后调用 `Database.reclaimFreeSpace()`（丢弃末尾的空闲页） | 一致（258,048） |
| `reuse` | 同样的表和删除，然后 `Database.setReuseFreePages(true)` 并再插入 300 行；分配器把这些行放到已释放的索引页，而不是扩展文件 | 一致（262,144） |
| `autonum` | 带 `setAllowAutoNumberInsert(true)` 的 LONG 自动编号主键：显式 id 5/100/3/7（较小的值不会推高水位线），随后生成 101-104，删除一个，再生成 105；另有一个无索引自动编号表混合显式和生成值 | 一致（245,760） |
| `fkadd` | `P(ID long pk, Name text(40))` 和 `C(ID autonumber pk, P_ID long, Note text(50))` 带数据行，然后建立带引用完整性的关系；在被拒绝的插入（外键找不到父行）前后各插入合法行；拒绝时文件不变且不消耗自动编号 | 一致（258,048） |
| `fkdel` | 一个级联删除关系（`P.ID → C.P_ID`）和一个限制删除关系（`N.ID → D.N_ID`）：删除 `P` 行会级联删除其子行；删除被 `N` 行引用的行会被拒绝；无子行的父行和子行可自由删除，然后添加更多行 | 一致（303,104） |
| `fkupd` | 一个级联更新关系（`P.ID → C.P_ID`）和一个限制更新关系（`N.ID → D.N_ID`）：父键修改会传播到子行；子行引用不存在的父行、以及限制关系的父键修改会被拒绝（文件不变）；随后允许的更新和更多行 | 一致（303,104） |
| `defaults` | 各列带 `DefaultValue` 属性的表（`LONG` 7、`LONG` 1.5 按银行家舍入为 2、`DOUBLE` 1.5、带引号和不带引号的 `TEXT`、`MEMO`、`BOOLEAN` TRUE、`LONG` -1）：省略列的行（应用默认值）、显式给值的行（忽略默认值）和显式 null 的行（添加时应用默认值），然后一次保留大部分列且从不应用默认值的更新 | 一致（221,184） |
| `required` | 一个 `Required` 的 `LONG` 列和一个关闭 `AllowZeroLength` 的 `TEXT` 列：合法行；随后因必填值为 null 和空字符串而被拒绝的插入（使用 Java 的消息，不添加行，不消耗自动编号）；再插入合法行 | 一致（221,184） |
| `validate` | 字段校验规则（带 `ValidationText` 的 `[Num] >= 0`、`Len([Code]) = 3`）和表行规则（带文本的 `[Start] <= [End]`）：合法插入；使用精确 Java 消息被拒绝的插入/更新（文件不变，不消耗自动编号）；随后更多合法行/更新 | V2003/V2007/V2010/V2016 一致 |
| `validateupdate` | 合法更新，加上被拒绝的字段/行规则更新（包括一次两列的行违规），然后更多合法更新和一次插入 | V2003/V2007/V2010/V2016 一致 |
| `multivalue`（仅 ACE） | `Lookup` + `Test(ID autonumber, Name, Tags 多值 TEXT 带值列表, Codes 多值 TEXT 带基于 `Lookup` 的 `RowSource`)` 和主键；3 行分别有 0、1 和 3+2 个值，然后修改一个值、删除一个、添加一个，并删除一行 | 一致（V2007 421,888；V2016 454,656） |
| `attachment`（仅 ACE） | `Test(ID autonumber, Name, Files attachment)` 有 4 行：0、1、2 和 1 个附件（原始 `png`/`jpg`/`zip` 负载，其中一行的附件大于一个数据页），然后重命名并重写一个附件、删除一个附件并删除持有该附件的行 | 一致（V2007 397,312；V2016 430,080） |
| `addcomplex`（仅 ACE） | `Test(ID autonumber, Name)` + 3 行，然后向现有表添加一个多值列和一个附件列（`ColumnBuilder.addToTable`），给已存在的行添加值（这些行获得新的复杂 id），并添加一个附件 | 一致（V2007 401,408；V2016 438,272） |
| `versionhistory`（仅 ACE） | `Test(ID autonumber, Name, Body MEMO 且 setAppendOnly(true))` + 主键；一行有初始 memo 值，三个带显式修改日期的版本，更新该行（新的 memo 值记录另一个版本），删除该行（jackcess 会保留其版本），第二行有一个版本 | 一致（V2007 385,024；V2016 421,888） |
| `vhadd`（仅 ACE） | 向已持有 5 行的表添加只追加 MEMO 列（`ColumnBuilder.addToTable`；现有行获得接下来的复杂 id），为每个现有行的 memo 值添加一个版本，然后添加一个带正文和版本的新行 | 一致（V2007 385,024；V2016 421,888） |
| `calc`（仅 V2010+） | `Test(ID autonumber, A LONG, B DOUBLE, S TEXT, D date)`，带计算列 `C=[A]+1`、`E=[B]*2`、`F=[S] & "x"`、`G=IIf([A]>0,"pos","neg")`、`H=[C]+[E]`（两个计算依赖）、`J="v=" & [A]`、`M=[S] & [S]`（MEMO，一个值在长值页）、`N=[A]*2`（NUMERIC）、`K=[D]`、`P=[A]>0`（BOOLEAN 结果）；六行包含 null、负数、零和分数 | 一致（V2010 364,544；V2016 385,024；V2019 454,656） |
| `calcupdate`（仅 V2010+） | 同一张表，然后对 `A` 做 `KEEP_VALUE` 更新、对 `B` 做整行更新、通过 `asUpdateRow` 修改 `S`、把 `A`/`S` 置 null 的更新，再添加两行（每次都重新计算计算列） | 一致（V2010 364,544；V2016 385,024；V2019 454,656） |
| `calcfloat`（仅 V2010+） | 计算 `MONEY`/`FLOAT`/`DOUBLE` 结果类型（`[A]/4`、`[A]*2`、`[A]/3`、`[A]/8`），覆盖行 `-2…3` 再加一个 null | 一致（V2016 380,928） |
| `noexpr` | `setEvaluateExpressions(false)`：带 `DefaultValue`、`Required`、`AllowZeroLength` 列的表，以及包含 null/空字符串的行（不应用默认值，`Required`/`AllowZeroLength` 仍然强制执行） | 一致（V2003/V2007/V2010/V2016 分别为 221,184 / 344,064 / 360,448 / 380,928 字节） |
| `noexprcalc`（仅 V2010+） | 同上，但带计算列：关闭表达式后 Java 既不计算它也不应用默认值，显式提供的计算值按原样存储 | 一致（V2010 360,448；V2016 380,928） |
| `tzutc` | 数据库时区为 `UTC`：从固定 epoch 时刻写入日期列，并处理 catalog 时间戳 | V2003/V2007/V2010/V2016 一致 |
| `tzneg` | 数据库时区为 `GMT-05:00`：同上，但使用负偏移 | V2003/V2007/V2010/V2016 一致 |

`tools\verify.ps1 -Scenarios "t1,rows" -Format V2007` 和 `-Format V2016` 也是
逐字节一致的（ACE 文件，例如 `rows` = 348,160 / 385,024 字节）。属性持久化场景在
`-Format V2007` 下逐字节一致（`dbprops`/`summaryprops` = 327,680，`tableprops` =
380,928，`propupdate` = 348,160 字节），在 `-Format V2016` 下也是（`dbprops`/
`summaryprops` = 364,544，`tableprops` = 417,792，`propupdate` = 385,024 字节）；
用 `-Format V2000` 对它们做抽查也全部一致。
行更新/删除场景（`delete,update,memoupdate,partialupdate,keeplong,rowops`）在
`-Format V2007` 和 `-Format V2016` 下同样逐字节一致（分别为 540,672 / 356,352 /
417,792 / 389,120 / 372,736 / 368,640 字节，以及 577,536 / 393,216 / 454,656 /
425,984 / 409,600 / 405,504 字节）。表变更场景在 `-Format V2007` 下逐字节一致
（`addcol,addindex,addboth,addmany` = 348,160 / 356,352 / 356,352 / 352,256 字节），
在 `-Format V2016` 下也是（`addboth` = 393,216 字节）。关系场景在 `-Format V2007`
下也逐字节一致（`relate,samerel,selfrel` = 376,832 / 421,888 / 356,352 字节），
并且用 `-Format V2000` 抽查 `relate`（147,456 字节）、用 `-Format V2016` 抽查
`selfrel`（393,216 字节）也一致。
链接表、空闲页和显式自动编号场景在 `-Format V2007` 下逐字节一致
（`linked,reclaim,reuse,autonum` = 368,640 / 380,928 / 385,024 / 364,544 字节），
在 `-Format V2016` 下也是（409,600 / 417,792 / 421,888 / 401,408 字节）。
外键强制场景在 `-Format V2007` 下同样逐字节一致（`fkadd,fkupd` = 376,832 /
421,888 字节）。
列默认值和校验器场景在 `-Format V2007` 下逐字节一致（`defaults,required` 各
344,064 字节），在 `-Format V2016` 下也是（各 380,928 字节）。
计算列场景仅适用于 V2010+，因为 jackcess 自身就拒绝对 V2003 和 V2007 使用计算列；
它们在 `-Format V2010`（`calc,calcupdate` 各 364,544 字节）、`-Format V2016`
（各 385,024 字节）和 `-Format V2019`（各 454,656 字节）下逐字节一致。

复杂列场景（`multivalue`、`attachment`、`addcomplex`、`versionhistory`、`vhadd`）
仅适用于 ACE：Jet 4 没有 `MSysComplexColumns`/`MSysComplexType_*` 支撑表。它们在
`-Format V2007` 和 `-Format V2016` 下逐字节一致（见上表）。Access 会在平面表名称中
放入随机 GUID，因此 `tools\verify.ps1` 会从 Java 文件中读回平面表名
（`java-src/ComplexInfo.java --flat-names`）并传给 Rust 生成器（`--complex-names`）；
版本历史列还有一个按列的类型表，名称中也含随机 GUID，由 `ComplexInfo --vh-names`
读回，并在平面表名之前放入同一列表（类型表先创建）。时间戳来自 `GetDates --tables`。
文件的其他内容都由两个写入器各自独立生成。

复杂列场景（包括 `versionhistory` 和 `vhadd`）在 `-Format V2019` 下也逐字节一致
（`multivalue` = 524,288，`attachment` = 503,808，`addcomplex` = 507,904，
`versionhistory`/`vhadd` = 491,520 字节）。（其他 `-Format V2019` 场景未验证；
那里的关系场景仍有差异，差异在 `MSysRelationships` 的 `DateCreate` 值中。）

`tools\verify-props.ps1` 还会比较解析后的属性 map：

- `empty.mdb`、`empty2003.mdb`、`empty2007.accdb`、`empty2016.accdb` 的数据库属性 —
  值一致（2/10/12/34 个属性）
- `MSysNavPaneGroups` 的表属性 — 一致

另外三个 harness 覆盖需要各自 fixture 的功能：

- `tools\verify-attach.ps1` — 附件列，包含原始文件类型
  （png/jpg/gif/zip/cab/docx/xlsx/xlsb/pptx）和 deflate 压缩类型
  （txt/csv/pdf/html/bin）、大写扩展名、空负载和大负载：在 V2007（450,560 字节）和
  V2016（483,328 字节）上逐字节一致。
- `tools\verify-link.ps1` — 一个目标数据库和一个包含两个链接表的 linker 数据库；
  通过 `LinkResolver.UNRESTRICTED` 读取链接中的行，并复制到本地表。linker 和目标文件
  在 V2003、V2007 和 V2016 上都逐字节一致。
- `tools\verify-expr.ps1` — 表达式引擎对 Jackcess 自身的计算列求值：3280/3280 个
  oracle 用例（包括自定义 `Format` 模式和错误用例）匹配，覆盖优先级、null/三值逻辑、
  整数除法与 `MOD` 符号、小数、日期、`LIKE`、转换函数以及向八种列类型的强制转换。
- `tools\verify-query.ps1` — 查询模型与 SQL 渲染对 Jackcess 自身的
  `QueryImpl.toSQLString()`：107/107 个合成 `MSysQueries` 用例（SELECT/连接/参数/
  交叉表/联合/追加/更新/删除/生成表/直通/数据定义，以及错误路径）。
- `tools\verify-linkwrite.ps1` — 一个目标数据库和一个带链接表的 linker 数据库；
  通过链接插入、更新（包括索引键修改）和删除行，然后读回并复制到本地表。两个文件在
  V2003、V2007 和 V2016 上都逐字节一致。
- `tools\verify-cursor.ps1` — 两套实现中的公共表扫描和索引游标 API（查找、迭代、
  当前行更新/删除、索引键修改）在同一数据库上运行：操作轨迹一致，数据库在
  V2003/V2007/V2016 上逐字节一致。
- `tools\verify-cursor-range.ps1` — 游标范围、保存点和 Iterable/EntryIterable builder
  （反向/空/前缀/null 边界、排他/包含端点、边界位置、恢复保存点语义、正向/反向
  iterable 和列子集、按列/索引选择 CursorBuilder 索引、非唯一 `findRow` 语义），
  操作轨迹 240 步一致，数据库在 V2003/V2007/V2016 上逐字节一致。
- `tools\verify-export.ps1` — `ExportUtil`/`ImportUtil`：15 个导出的文本文件逐字节
  一致（5 组选项 × 3 种格式），24 个导入数据库逐字节一致（7 个导入场景 × 3 种格式，
  外加 3 个源数据库），覆盖带引号字段、内嵌换行/分隔符、null、CRLF 和显式列类型。
- `tools\verify-crypt.ps1` — Jet 4 数据库编码和数据库密码，对照 jackcess-encrypt
  5.0.2：编码文件、密码文件和密码+编码文件与 Java 逐字节一致；编码/解码以及
  设置/移除密码的往返都返回精确的原始字节，通过 codec provider 读取的行匹配，
  错误密码的拒绝行为匹配，就地/路径两种形式一致。
- `tools\verify-crypt-office.ps1` — ACE（`.accdb`）加密：agile
  `OFFICE_2010`/`OFFICE_2013` 读写、ECMA 标准，以及用于读取和移除加密的旧 RC4
  provider。在确定性模式下（固定 salt/key/verifier 和编码 key，对应 Java 的
  `AgileEncryptionWriter` API）加密文件逐字节一致；在随机模式下，双方都能把对方的
  文件解密为精确的明文基线，并读取相同的行，错误密码会以 Java 的
  `Incorrect password provided` 被拒绝。
- `tools\verify-querydb.ps1` — 集成到 `Database` 中的查询模型（读取 `MSysObjects`
  类型 5 行 + `MSysQueries`）：14 个查询（带参数/WHERE/ORDER BY 的 SELECT、
  GROUP BY/HAVING、连接、交叉表、联合、追加/更新/删除/生成表、直通、隐藏和系统查询）
  在 V2003/V2007/V2016 中产生一致的 SQL 和元数据 dump；元数据 dump 还覆盖表/列/索引
  元数据，以及构造的链接表（类型 6）和 ODBC 表（类型 4），包括
  `TableDefinition has no data access` 拒绝。
- `tools\verify-namemap.ps1` — `NameMap`/`TableNameMap`/`QueryNameMap` 二进制 blob：
  真实 map（重命名、Unicode、删除/不匹配的记录）、属性缺失场景和 11 个畸形 blob
  错误场景，由两套实现解析：V2003/V2007/V2016 的 dump 一致。
- `tools\verify-exportall.ps1` — `ExportUtil.exportAll` 以及流式/覆盖/追加形式：
  V2003/V2007/V2016 下 231 个文本文件一致，51 个 manifest 一致（多表数据库、
  文件名转义包括 `R&D #2 (100%)` 和 Unicode 名称、空数据库、恶意名称、选项集）。
- `tools\verify-realquery.ps1` — 来自 jackcess 测试语料的**真实 Access 创建文件**：
  `queryV1997/V2000/V2003/V2007/V2010`（各 9 个查询）和 `linkedOdbcV2007.accdb`
  的 56 个查询产生一致的名/类型/标志/参数/所有者访问/SQL dump，并且该文件中的真实
  ODBC 链接表（`LINKED_ODBC`、连接字符串、20 列、5 个索引、无数据错误）匹配。
- `tools\verify-jet3.ps1` — Jet 3 / Access 97 **读取**，对照 9 个真实 V1997 文件
  加 18 个 V2000/V2003 交叉检查：27/27 个规范 dump（定义、元数据、每一行、长值、
  溢出、索引/条目）一致。
- `tools\verify-ole.ps1` — `OleBlob`/OLE 复合文件：真实 `blobV2007.accdb`
  （193 行，11 个值，包括 Word/PDF/Excel 复合存储）、12 个包装 fixture、24 个
  原始/截断/损坏的复合文件和合成的 V2000/V2003/V2007 数据库全部匹配（比较 SHA-256）；
  v4（4096 字节扇区）路径用生成的 fixture、DIFAT 和畸形用例验证（11 个 section）。
- `tools\verify-errorhandler.ps1` — `ErrorHandler`/`DebugErrorHandler` 作用于真实文件
  中损坏的 TEXT 行和损坏的 MEMO 行（无 handler 失败、替换值、表/数据库/游标覆盖、
  抛异常的 handler、不支持的列和 `TableDefinition has no data access` 场景）：
  回调轨迹一致（96 行）。
- `tools\verify-queryprops.ps1` — 查询属性 map 和查询名称 map：
  10 个数据库 / 121 个查询，113 个带属性（276 条记录）和 53 个名称 map
  （206 条记录），外加 Java 生成的值类矩阵和 4+4 个畸形用例：dump 一致。
- `tools\verify-customlink.ps1` — 目标被移动后的 `CustomLinkResolver`：
  相对/绝对映射、大小写不同的名称、loader 形式、缺失映射和临时数据库 resolver；
  在 V2003/V2007/V2010/V2016 上读取 dump 一致，linker 文件保持不变。
- `tools\verify-jet3-crypt.ps1` — Jet 3 **编码**读取：用 Java 页变换（通过反射）编码的
  9 个真实 V1997 文件与 Rust 编码器的输出逐字节一致，Java/Rust 读取相同的行，两者都能
  解码回原始字节，构造的 Jet 3 密码文件逐字节一致，并且两者都以 Java 的消息拒绝公共
  编码/设置密码 API。
- `tools\verify-datetime.ps1` — `DateTimeType`（DATE / LOCAL_DATE_TIME；5.0.4 没有
  STRING 模式）和 `ColumnOrder`，作用于真实文件（`oldDatesV2007` 含 1392-1582 年日期、
  `common1V2003/V2010`）和生成的 V2003-V2016 数据库：值类/millis/存储位和列顺序 dump
  一致，两种模式下的写入都逐字节一致（两种模式只在亚毫秒值上有差异）。
- `tools\verify-memdb.ps1` — 内存中的 `MemFileChannel`/`MemDatabase`：
  Java 内存写出的字节等于磁盘文件，Rust 内存写出的字节与 Java 磁盘文件在
  V2003/V2007/V2010/V2016 上逐字节一致（491,520 / 610,304 / 626,688 / 647,168 字节），
  包括重新打开这些字节。
- `tools\verify-realcrypt.ps1` — **真实** Access/jackcess-encrypt 文件：
  `db-enc.mdb`/`db97-enc.mdb`（编码，密钥被精确恢复并重新编码）、
  `pwd-set`/`pwd2-set`（密码 `testpassword`，SID 集和 page-0 字段一致），以及
  `db2007-enc`/`db2007-oldenc`/`db2013-enc`/`db-nonstandard`（agile/RC4/AES，
  密码 `Test123`/`1234`/`password`）：读取、解密字节和错误密码失败全部一致。
- `tools\verify-msisam.ps1` — Microsoft Money `.mny`：两种编码（旧的文件派生 key 和
  现代 MD5/SHA-1 哈希密码）、5 个真实文件（873/1691/3192/876/3192 行，402-504 个索引
  定义）、密码（`Test12345`）、错误密码和创建/写入拒绝：47/47 项检查一致。
- `tools\verify-codepage.ps1` — Jet 3 代码页：全部 109 个 JDK 代码页（87 个单字节 +
  22 个多字节，包括 IBM EBCDIC DBCS 状态机）：10,886,038 次与 JVM 的精确解码/编码
  比较，外加使用补丁代码页的真实文件文本 dump（EBCDIC 头文件已证明 Java 也无法打开，
  因此通过文档化的 charset override 检查字符集）。
- `tools\verify-apisurface.ps1` — 18 个 Database/Table 读取 API dump（真实文件，包括
  一个 `.mny`、生成的 V2003-V2016、版本历史/多值/附件）一致，包括异常。
- `tools\verify-jet3-depth.ps1` — Jet 3 深度读取：在 9 个 V1997 文件中，103 个表、
  706 列、162 个索引、5,062 个条目、1,905 个属性、680 行 ACE、2 个关系，以及 153 次
  写入拒绝探测（114 次被拒绝，文件 SHA-256 不变）：两份 dump 一致。
- `tools\verify-miscdeg.ps1` — `Double.toString`/`Float.toString` 作用于 237,945 个
  困难值（所有次正规数、随机位模式、10 的幂、ties），外加 Java 序列化二进制导入、
  `ColumnFormatter`（835 行格式矩阵）和无临时文件的 in-memory 数据库检查：文本/字节
  全部一致。
- `tools\verify-timezone.ps1` — 带 DST 和历史偏移的 IANA 时区
  （UTC、GMT-05:00、America/New_York、Europe/Berlin、Asia/Shanghai、
  Australia/Lord_Howe、Pacific/Apia）：跨 DST 间隙/重叠、1901/1970/2038 边界和
  1901 年前 LMT 值的 wall-clock <-> instant 转换，以及所有四种格式上逐字节一致的
  写入/读取（144 项检查）。
- `tools\verify-zonedates.ps1` — 上述时区的带时区 DATE 模式值、属性、索引查找和读取：
  113 项检查一致。
- `tools\verify-rowapi.ps1` — Row/表/游标 API：Row 内容、id、类型化 getter
  （包括 null/错误类型路径）、getBlob、DATE 模式、map 相等性、迭代顺序，以及一个生成的
  添加/更新/删除/迭代场景：dump 一致，数据库在 V2003/V2007 上逐字节一致。
- `tools\verify-dbapi.ps1` — DatabaseBuilder/静态工厂、builder 创建的数据库
  （四种格式都逐字节一致）、打开选项（只读、charset、命名/固定时区）、加密打开、
  表/元数据 iterable、TableMetaData.open/definition 和 close 语义（32 项检查）。
- `tools\verify-objapi.ps1` — Column 访问器（行值、SQL 类型、版本历史、复杂信息、
  校验器）、Index/Index.Column 访问器、Relationship 表/列解析和 TableNameMap
  添加/删除/保存（保存后的文件逐字节一致，V2003/V2007/V2016）：43 项检查。
- `tools\verify-utilapi.ps1` — DataType 字节/SQL/单位转换、PropertyMap put 形式
  （blob 字节）、OleBlob.Builder 布局、EvalConfig 默认值和所有校验器结果/消息：
  3,738 行一致。

catalog 时间戳被确定化：harness 读取 Java 生成器写入的 `DateCreate` 值，并把相同的时刻
传给 Rust 生成器（`--now-ms`，每个 catalog 对象一个值）。

## 工作区结构

| 路径 | 内容 |
| --- | --- |
| `src/` | Rust crate（库代码 + `mdbgen` / `mdbdump` / `jetcrypt` / `zonedategen` 四个发布 bin；`src/bin` 下的验证生成器源码也保留，但由 `dev-bins` feature 控制，默认不构建） |
| `resources/z/` | 编译期嵌入的 zlib 压缩资源（模板、排序表、代码页表，约 1.5 MB）；原始文件不提交 |
| `Cargo.toml` / `Cargo.lock` | crate 元数据和锁定依赖 |
| `README.md` / `README.en.md` | 中文 / 英文说明 |
| `CHANGELOG.md`、`LICENSE`、`NOTICE` | 发布记录、Apache-2.0 许可和上游归属 |
| `fetch-external.ps1` | 可选：下载开发验证用的 Java 参考库、源码和真实语料（不包含 `tools/`、`java-src/` 与测试 fixture） |
| `.github/workflows/` | 三系统 CI 与发布打包 |

## 环境

构建和测试本库只需要 Rust 工具链，crate 没有 C/系统库依赖。

Java 参考工具、`tools/` 下的 harness、`java-src/`、`external/` 以及 Java-vs-Rust 测试 fixture 只存在于开发树，**不随仓库发布**。`fetch-external.ps1` 可以恢复其中的 Java 参考库、源码和真实 Access/加密语料；完整验证在开发树中执行 `tools\verify-all.ps1`。

默认 feature 下的 Cargo 依赖：

- 核心：`chrono`、`chrono-tz`、`encoding_rs`、`bigdecimal`
- `crypto`：`aes`、`sha1`、`sha2`、`md-5`、`rc4`、`base64`、`getrandom`、`roxmltree`

数据库引擎的读写核心本身只依赖 std。

## 架构（对应 Java 类）

| 模块 | 对应的 Java 类 |
| --- | --- |
| `src/bytebuf.rs` | `java.nio.ByteBuffer` 的等价实现（绝对/相对访问、字节序） |
| `src/byteutil.rs` | `ByteUtil`（3 字节整数、变长整数） |
| `src/format.rs` | `JetFormat`、`Database.FileFormat`、`SortOrder` |
| `src/datatype.rs` | `DataType` |
| `src/datenums.rs` | Access OLE 日期 double、`LocalDateTime`、时区 |
| `src/pagechannel.rs` | `PageChannel`（页 I/O、分配、header mask、全局 usage map） |
| `src/usagemap.rs` | `UsageMap`（内联/引用 map、提升、全局 map） |
| `src/column.rs` | `ColumnImpl`（定义、值编解码、文本压缩、长值） |
| `src/table.rs` | `TableImpl`（表定义、数据页、行读写、null mask） |
| `src/tablecreator.rs` | `TableCreator` / `TableBuilder` / `IndexBuilder`（包括创建时的表/列属性） |
| `src/tableupdater.rs` | `TableUpdater` + `TableImpl.mutateAddColumn` / `mutateAddIndexData` / `mutateAddIndex` / `addUsageMaps`（向现有表添加列和索引） |
| `src/complex.rs` | `ComplexDataType` / `ComplexColumnDesc` / `ComplexColumnInfo` / `MultiValueColumnInfo` / `AttachmentColumnInfo` / `VersionHistoryColumnInfo`、`ComplexValue` / `SingleValue` / `Attachment` / `Version`、`ComplexValueForeignKey`、`ComplexColumnSupport` 以及附件数据包装器（`ComplexColumnCreator` 位于 `tablecreator.rs`） |
| `src/index.rs` | `IndexData` / `IndexImpl`（条目、排序、键编码） |
| `src/indexpagecache.rs` | `IndexPageCache`（B 树页、分裂、嵌套、延迟写入） |
| `src/indexcodes.rs` | `GeneralLegacyIndexCodes` / `General97IndexCodes` / `GeneralIndexCodes` |
| `src/property.rs` | `PropertyMap` / `PropertyMaps` / `PropertyMapImpl`（MR2/KKD blob、`save` 以及 builder 的属性顺序） |
| `src/relationship.rs` | `RelationshipBuilder` / `RelationshipImpl` / `RelationshipCreator` 以及 `DatabaseImpl` 对 `MSysRelationships` 的处理 |
| `src/fkenforcer.rs` | `FKEnforcer`（外键检查、级联更新/删除/置 null、共享更新深度）和 `util.Joiner` |
| `src/linked.rs` | `util.LinkResolver`（`DEFAULT`/`UNRESTRICTED`）和 `DatabaseImpl` 的链接表读取 |
| `src/deflate.rs` | zlib 1.3.1 `deflate_fast` level 3 + zlib 包装和一个 inflater（附件压缩） |
| `src/expr.rs` | `impl.expr.Expressionator`（tokenizer、parser、evaluator、值模型和内建函数） |
| `src/query.rs` | `query.Query` + `impl.query.QueryImpl` 以及具体查询类（SQL 渲染） |
| `src/cursor.rs` | `Cursor`/`TableScanCursor`/`IndexCursor` 及其查找、更新和删除 |
| `src/export.rs` | `util.ExportUtil` / `util.ImportUtil`（分隔文本） |
| `src/crypt/` | `CodecHandler`/`CodecProvider`、Jet 页编解码/编码/密码以及 Office agile/ECMA 编解码（`jackcess-encrypt` 5.0.2） |
| `src/namemap.rs` | `NameMap`/`TableNameMap`/`QueryNameMap`（显示名 blob） |
| `src/ole.rs` | `util.OleBlob` / `OleUtil` 以及 OLE 复合文件读取器 |
| `src/mem.rs` | `util.MemFileChannel` 和内存数据库（`MemDatabase`） |
| `src/row.rs` | `Row`（有序列 map + 类型化 getter）以及表/游标行迭代器 |
| `src/builder.rs` | `DatabaseBuilder`、静态 builder 工厂和 `TableIterableBuilder` |
| `src/validator.rs` | `util.ColumnValidator` / `ColumnValidatorFactory` 以及校验器链 |
| `src/crypt/msisam.rs` | `MSISAMCryptCodecHandler`（Microsoft Money 页 key） |
| `src/database.rs` | `DatabaseImpl`（打开/创建、系统 catalog、表查找、数据库/摘要/用户定义属性） |

## Java API 兼容层

原有的 Rust API 保持不变；在其之上补充了 `com.healthmarketscience.jackcess` 的公开
形态。Java 的重载无法共用同一个 Rust 名称，因此映射为不同且已文档化的名称（各模块文档
中列出自己的映射表）：

| Java | Rust |
| --- | --- |
| `Row`（Map + 类型化 getter）、`Row.getId()` | `row::Row`（`get_string` … `get_blob`）、`get_id` |
| `Table.asRow/asUpdateRow` | `Table::as_row` / `as_update_row` |
| `Table.addRowFromMap/addRowsFromMaps` | `Database::add_row_from_map` / `add_rows_from_maps`（以及 `Table` 形式） |
| `Table.updateRow(Row)` / `deleteRow(Row)` | `*_row_object`（按位置的重载保留原名） |
| `Table.iterator/stream` | `TableRowIter`、`Database::row_iter`、`Table::stream` |
| `Table.newCursor/getDefaultCursor` | `Table::new_cursor` / `default_cursor` |
| `Cursor.iterator/stream`、`getNextRow(Collection)` | `CursorRowIter`、`Cursor::stream`、`get_next_row_object(_columns)` |
| `DatabaseBuilder` + 静态工厂 | `builder::DatabaseBuilder`、`new_table/new_column/new_index/new_primary_key/new_relationship` |
| `Database.iterator/stream/newIterable/newTableMetaDataIterable` | `Database::tables`/`for_each_table`、`new_iterable`、`new_table_meta_data_iterable` |
| `TableMetaData.open/getTableDefinition` | `TableMetaData::open` / `get_table_definition` |
| `Column` 对象访问器 | `get_table(&Table)`、`get_database`、`get_row_value`、`get_sql_type`、`get_version_history_column`、`get_complex_info`、校验器访问器 |
| `Index`/`Relationship` 访问器 | `get_table`、`get_properties`、`new_cursor`、`get_from/to_table/columns` |
| `TableNameMap.addColumn/removeColumn/save` | `add_column`、`remove_column`、`save_for_table` |
| `Database.close/getFile/getZoneInfo/getEvalConfig/validator factory` | 对应的 snake_case 形式 |
| `Database.getRelationships/getSystemRelationships` | `get_relationships` / `get_system_relationships` |
| `ColumnValidator(Factory)` | `validator::ColumnValidator`、`ColumnValidatorFactory`、`DefaultColumnValidator`（由 insert/update 执行） |
| `OleBlob.Builder` / `Table.newBlob` | `ole::OleBlobBuilder` / `Table::new_blob` |
| `Column.AUTO_NUMBER` | `column::AUTO_NUMBER` + `is_auto_number_value`（添加时生成，更新时保持） |
| `DataType`/`PropertyMap` 便利方法 | `try_*`、`from_sql_type*`、`put_typed*`、`get_value_default`、`stream` |

已知的 Rust 形态差异：Java 的 `Row`/`Cursor` 迭代器抛出 `UncheckedIOException`，Rust
迭代器则产生 `Result<Row>`；`Row` 的类型化 getter 返回 `Result<Option<T>>`；
`java.util.Date` 由 `Value::DateBits` 加一个 millis 辅助方法表示；`Table` 没有 database
反向引用，因此基于 Row 的添加/更新/删除位于 `Database` 上，表相关的辅助函数显式接收
database；`EvalConfig` 已暴露，但求值器仍使用内置的 temporal/numeric/function 默认值。

## 端到端比较之外的测试证据

> 同样，本节引用的 harness、oracle 和 fixture 都位于开发树，不在发布仓库中。


- `cargo test --lib` — 518 个测试（517 通过，1 个大型 sweep 被忽略）。其中包括：
  - 针对 11 种索引形状和 600 行的索引条目字节与索引页（`indexpagecache::tests::many_rows_match_java_index_pages`：全部 25 个索引页一致），
  - 文本排序：387,204 个生成用例（每个 UTF-16 代码单元 × 3 种排序 × 升序/降序，外加随机字符串）与 Java 逐字节一致，
  - 属性 blob：24 个 Java 生成的 fixture，加上模板中 35 个真实 `LvProp` blob，往返后逐字节一致，
  - 属性持久化（`database::tests::*`、`property::tests::*`）：`dbprops` 场景的数据库/摘要/用户定义组，以及 `tableprops` 场景的创建时和更新后的表/列组（其 MEMO 属性使 blob 达到 12,312/12,288 字节，并跨多个长值页存储）与 Java 库写出的 blob 匹配（fixture 由 `java-src/PropFixtures.java` 重新生成），复现了 Java builder 的 `HashMap` 属性顺序（对若干名称集做了固定），从文件读回的 `LvProp` 值等于 blob，被清空的组存储为 null `LvProp`，保存时会创建缺失的 `SummaryInfo` catalog 行，
  - 列默认值和属性驱动的校验器（`column::tests::*`、`table::tests::*`）：Java `DefaultValue` 表达式引擎接受的字面量形式（带引号/不带引号文本、整数、像 `1.5` 这样在 LONG 列中按银行家舍入为 2 的小数、布尔常量/数值、`Null`，以及本来会被看作数字的文本），带有精确的 `generateDefaultValue`/`toInternalValue` 强制转换；对 `=` 表达式、日期字面量、日期列以及 jackcess 必须舍入的 `NUMERIC`/`MONEY` 字面量给出 `Unsupported` 报告；`Required`/`AllowZeroLength` 消息包括 `(Db=…;Table=…;Column=…)` 上下文；默认值在添加时应用、更新时不应用；被拒绝的插入/更新不改变行集和自动编号计数器；属性保存会把校验器重新应用到缓存的表，
  - 新索引根页缓冲区与 Java 逐字节一致，
  - 字节层面的行更新/删除机制：行目录中的删除/溢位标志、未改变的页空闲空间记录、表定义的行数、更新时未改变的索引唯一条目计数，以及 4 字节溢出指针和头行其余部分被清零（`table::tests::*`），
  - `KEEP_VALUE` 部分更新（`table::tests::*`）：保留所有列的更新会重写完全相同的行字节（定长值由解码形式重新编码，变长值和长值定义被复用）；被保留的多页 MEMO 在五次更新中保留其 12 字节长值定义（同一指针）且不分配页，文件大小不变，而替换值会分配新的长值页；保留值在唯一/非唯一索引更新中按旧值处理（唯一条目计数不变，重复唯一键被拒绝并恢复被移除的条目）；三个入口（`update_row_partial`、`update_value`、`update_row_from_map`，缺失列保持、未知键忽略）结果一致；布尔值（null mask）和保留的 null 保持不变（更新从不生成列默认值）；外键强制把保留的外键列视为未改变的旧值（`fkenforcer`）；新行拒绝 `KEEP_VALUE`，
  - 字节层面的表变更机制（`tableupdater::tests::*`）：`add_column` 后的定义长度和 max/var/列计数，新列的编号/id/变量索引/固定偏移/标志/长度，由新长度重新计算的空闲空间，`add_index` 的定义增长（索引数据块 + 逻辑索引块 + 名称），新的根页及其空闲空间，写入定义的已填充唯一条目计数，等价索引复用现有底层索引数据，以及大到溢出到续页的定义（重新打开数据库后可读），
  - 行重编码：Java 生成的 `MSysObjects` 表中每个非长值行都重编码为完全相同的字节（`mdbdump <file> MSYSOBJECTS --reencode`），
  - 字节层面的关系创建（`relationship::tests::*`）：`MSysRelationships` 行负载及其精确重编码行字节，两个支撑索引的外键引用（表类型、另一个索引号、另一个表页、级联标志、索引类型）直接从表定义页读出，索引数据标志（主键侧复用主键索引数据时保持 `0x89`；次级侧得到普通的 `0x80` 索引数据），精确的定义长度和生成的索引名（`.rB`，被占用时走到 `.rE` / `.ra`，以及以关系名作为次级索引名），
  - 复杂列（`complex::tests::*`）：多值列的 `Test` 表定义页和平面表定义页与 Java 库写出的字节匹配（fixture `resources/complex_tdefs.bin`，由 `java-src/ComplexFixtures.java` 重新生成；fixture 的随机平面表名存放在 `resources/complex_flat_name.txt` 中并由测试复用），`COMPLEX_TYPE` 类型字节和四个排序字节中的复杂 id，平面表的 `_Tags` 外键列携带复杂外键 ext 标志，catalog 标志（带该列的表上为 `0x40000`，平面表上为 `0x800A0000`），`MSysComplexColumns` 行值，主行的复杂 id 和复杂自动编号计数器；向现有表添加复杂列会创建支撑表、标记 catalog 标志、给现有行分配接下来的复杂 id，并保持该列可用；多值 API（`add_multi_value`、`update_multi_value`、`delete_value`、`multi_values`）经重新打开后往返；附件数据包装器编码/解码原始负载并拒绝 deflate 压缩类型；附件列的平面表具有 `_Files,FileURL,FileName,FileType,FileData,FileTimeStamp,FileFlags,Test_Files` 形态；版本历史列（`version_history_definitions_match_java_fixture`）与 Java 表定义、按列的 `MSysComplexTypeVH_<guid>` 类型表定义和平面表定义逐字节匹配（fixture `resources/complex_vh_tdefs.bin` + `resources/complex_vh_names.txt`，也由 `java-src/ComplexFixtures.java` 重新生成），包括类型表的 `Body,Modified_...` 形态、平面表的 `_VersionHistory_...,Body,Modified_...,Test_VersionHistory_...` 形态、catalog 标志（类型表上为 `0x80030000`）、`VERSION_HISTORY` ext 标志以及 memo 列及其表的 `AppendOnly` 属性；版本 API（`add_version`、`versions` 最新在前、被拒绝的更新/删除）经重新打开后往返，并且第二个只追加 memo 列（或只追加的非 memo 列）在创建和添加列两条路径上都被拒绝，
  - 空闲空间处理（`pagechannel::tests::*`）：除非启用 `setReuseFreePages`，已释放的页不会再次分配（启用后最低空闲页优先，复用的分配不会使文件增长），`truncateFreeSpace` 丢弃文件末尾连续的空闲页并报告数量，
  - 链接表创建（`database::tests::*`）：合成对象 id 接在模板和关系的 id 之后，类型 6 catalog 行保存外部数据库/表名，访问控制条目携带父对象的 SID，查找条目在重新打开后仍然存在（因此重复名称检查适用于已有链接表），名称检查拒绝重复、空白链接数据库名、非法字符和前导空格，
  - 显式自动编号插入（`database::tests::*`）：数据库和表层策略（包括两个方向的表覆盖）、显式值对水位线的调整（较小值被忽略）、生成值在已用最大值之后继续、删除行不会降低水位线，以及重新打开后保存的 `OFFSET_NEXT_AUTO_NUMBER`，
  - 外键强制（`fkenforcer::tests::*`）：索引键查找（`IndexData::find_rows_by_entry`：精确键找到共享该条目的行，包括重复的非唯一键；不在索引中的键落到下一个条目；超过末尾的键什么也找不到），插入的外键没有父行时以精确的 `ConstraintViolationException` 消息文本拒绝（包括 `Foreign Key from C[P_ID] (secondary) to P[ID] (primary) (Db=…)` 描述和 `Adding new row [3, 99, bad] violates constraint` 前导语），之后文件不变且不消耗自动编号；删除和更新的级联/限制行为（拒绝后文件不变）；null 和大小写不敏感的文本外键；自己是自己父行的自关联行（添加和删除时允许）；以及 `setEnforceForeignKeys(false)`。
  - 计算列（`table::tests::*`）：Java 写出的表定义页和第一个数据页（fixture `resources/calc_fixture.bin`，用 `Gen calcfixture` 重新生成）逐字节匹配，包括 `ResultType`/`Expression` 属性、计算列 ext 标志、长度规则和包装后的值编码；依赖顺序和依赖环、V2010 之前的拒绝消息、向现有表添加计算列，以及会重新计算计算列的 `KEEP_VALUE` 更新；`mdbdump --reencode` 现在会跟随溢出指针并复用非内联长值定义，因此计算列行可一致地重编码。
  - zlib/附件压缩（`deflate::tests::*`）：341/341 个 Java `Deflater(3)` 流逐字节一致（3.2 MB 输入，覆盖每种块类型、Huffman 边界情况、长匹配和窗口滑动），94/94 个编码附件负载（原始和压缩、大写扩展名、空和大）一致且往返正确。
  - 表达式引擎（`expr::tests::*`）：434/434 个 Java oracle 用例（397 个值，37 个解析/求值/算术错误）匹配，语料在 `resources/expr_ref/` 中，由 `java-src/ExprOracle.java` 重新生成。
  - 链接表（`database::tests::*`）：创建/重新打开后的链接元数据，默认 resolver 的拒绝消息会指出 linkee 文件，不受限 resolver 可打开并读取目标，只读 linker 以只读方式打开 linkee，通过链接读取不会修改 linker；通过链接的链接**写入**（插入/更新（索引键修改、`update_value`、map 更新）/删除）落到 linkee 中且 linker 字节不变，linkee 的约束/默认值/校验器生效，schema 写入（`add_column`/`add_index`/自动编号策略）会修改 linkee，只读 linkee 拒绝写入。
  - 查询模型（`query::tests::*`）：107/107 个合成 `MSysQueries` 用例与 `QueryImpl.toSQLString()` 匹配，行 fixture 和期望文本位于 `resources/query_ref/`，由 `java-src/QueryOracle.java` 重新生成。
  - 计算 `MONEY`/`FLOAT` 结果类型（`calcfloat`）：向 `Value.Type.DOUBLE` 的强制转换以及 currency/float 写入路径与 Java 逐字节一致（V2010/V2016/V2019）。
  - 游标 API（`cursor::tests::*`）：升序、降序、唯一和非唯一索引上的索引条目排序/查找，表扫描查找语义，当前行有效性，以及通过两种游标进行的更新/删除。
  - 分隔文本导入/导出（`export::tests::*`）：转义/引号、分隔符、多行值、null、CRLF、解析器的过期匹配器怪癖、Java 数字/日期/二进制格式化、批量写入和建表。
  - 数据库设置（`database::tests::*`、`datenums::tests::*`）：`setEvaluateExpressions` 精确控制 Java 对应部分（计算列、默认值、表达式校验器；`Required`/`AllowZeroLength` 保持生效），固定偏移时区（UTC/GMT-05:00）控制 catalog 和日期转换，以及存储的 `ColumnOrder`/`DateTimeType` 设置。
  - OLE 日期解码现在会把舍入后的整秒纳秒带入秒字段（类似 `Duration.ofSeconds`），`Value::Byte` 按 Java 的有符号 byte 处理（0xFF 写为 -1），二者都有回归测试。
  - Jet 加密（`crypt::tests::*`）：RC4 页变换、header mask 和 key 处理、密码字段/SID 重新掩码和编码 key fixture，全部固定为 Java 推导值（`tools/crypt_fixtures.txt`）。
  - Office 加密（`crypt::office::tests::*`）：agile XML 描述符、密码 key 派生、块链接、verifier 和标准/RC4 provider，固定为 Java 推导的 fixture（`tools/office_crypt_fixtures.txt`），外加确定性写入器的字节。
  - 查询集成和元数据（`database::tests::*`、`table::tests::*`）：按对象 id 分组 `MSysQueries` 行、catalog 类型 5 查找、表/列/索引元数据字段（包括类型 4/6 行和仅定义的拒绝）。
  - 校验规则（`column::tests::*`、`table::tests::*`）：从属性加载规则，精确的 `Invalid column value '<v>'` / `Invalid row` / `ValidationText` 消息，"condition evaluated to Null" 错误，不写入/不消耗自动编号的拒绝，以及 evaluateExpressions 门控。
  - 游标范围/保存点/iterable（`cursor::tests::*`）：反向/空/前缀边界、边界位置、包含标志、保存点恢复语义，以及带列子集的正向/反向 iterable。
  - 表达式引擎的扩展内建函数（`expr::tests::*`）由 `resources/expr_ref/` 中的 3280 个用例 oracle 语料覆盖。
  - 名称 map（`namemap::tests::*`）：表/查询 map 的 blob 解析、记录、不匹配项排序、Unicode 名称和畸形 blob 错误，全部固定为 Java 所解析的相同字节（`resources/namemap_ref/`）。
  - `exportAll`/流式（`export::tests::*`）：表过滤、文件名转义/冲突、覆盖/追加和空数据库场景。
  - GUID 自动编号（`table::tests::*`）：v4 形态/唯一性、显式策略和错误消息、更新/保持、显式插入页字节和重复自动编号列拒绝。
  - Jet 3（`format::tests::*`、`table::tests::*`）：跳转表偏移（短和跳转）、删除/溢出行为、MEMO 长值、条目、只读拒绝、windows-1252 解码、VERSION_3 常量和真实 fixture。
  - OLE（`ole::tests::*`）：header/FAT/DIFAT/mini-FAT/directory 解析、大流和小流、嵌套存储、循环/截断/块错误、内容包装器和与 Java/POI 兼容的消息。
  - 真实查询 fixture（`query::tests::*`）：6 个 Access 创建的数据库（101 个查询）从其存储行重建，并与 Java 比较。
  - 错误 handler（`table::tests::*`、`database::tests::*`）：默认重新抛出、替换值、数据库/表/游标覆盖优先级、回调上下文和消息、抛异常的 handler、不支持列的原始值。
  - 查询属性（`query::tests::*`）：catalog `LvProp` 加载、空和畸形 map、名称 map 解析、保存往返和真实文件 fixture（121 个查询）。
  - 自定义链接 resolver（`linked::tests::*`）：map/loader 形式、相对和绝对解析、大小写不敏感名称、缺失映射和临时数据库 loader。
  - Jet 3 加密（`crypt::tests::*`）：Jet3 页变换和 key 布局、不含创建日期掩码的密码字段，以及真实 V1997 编码/读取/解码集成。
  - 索引压力测试（`indexpagecache::tests::*`）：页缓存 LRU 清除顺序、陈旧页尾字节，以及 900 行多索引树在分裂和批量删除后重新解析其页。
  - 日期类型（`database::tests::*`、`datenums::tests::*`）：DATE/LDT 读写转换、整毫秒不变量的逐字节一致（438,000 个值）、旧日期往返和列顺序视图。
  - 内存数据库（`mem::tests::*`）：增长/截断/定位/越过末尾读取、只读写拒绝，以及通过内存的数据库往返。
  - 真实加密文件（`crypt::real_file_tests::*`）：Jet 3/4 编码 key、密码字段、SID 集、Office 解密后的 SHA-256。
  - API/异常一致性（`database::tests::*`、`table::tests::*`、`index::tests::*`、`bytebuf::tests::*`）：批量更新包装、系统表 catalog 日期/名称 map、被引用索引校验、ACE 行、Java NIO 位置措辞和排序顺序 id。

- `java-src/idx/.../IdxOracle.java` 生成索引测试使用的 Java 参考值。

## 范围与已知缺口

**范围策略：** 本移植只实现 jackcess 5.0.4（加密部分为 jackcess-encrypt 5.0.2）实际
暴露的行为。参考库没有提供 API 的任何内容（VBA/`MSysAccessObjects`、删除关系、删除
列/索引、JDBC/ODBC 数据访问、MSISAM/Jet 3 写入、旧 Office 加密写入器、
`Partition`/`D*` 聚合、`importResultSet`）都**不在范围内，也不属于移植缺口**；既未实现
也不计划实现。下面的列表只覆盖参考库确实具有的行为。

已实现并验证：Jet 4（Access 2000/2003）和 ACE（2007/2010/2016）数据库创建，带
列/索引的表创建，向现有表添加列和索引（`ColumnBuilder.addToTable` /
`IndexBuilder.addToTable`：表定义增长、列 id/偏移分配、索引数据创建与填充、定义跨页
分裂），行插入（包括长 MEMO/OLE 值、压缩文本、所有标量列类型），行更新/删除（通过
溢出行的就地重写和重定位、长值页、索引条目替换、表定义行数），`KEEP_VALUE` 部分行更新
（`Column.KEEP_VALUE` / `Value::Keep`：定长列保留旧的解码值，变长列还会复用其页内原始
字节，因此被保留的 MEMO/OLE 值保留现有长值定义和页而不是重新写入；保留值在索引更新、
外键检查和校验器中按旧值处理；更新从不生成列默认值；以及
`updateValue`/`asUpdateRow`/`update_row_from_map` 入口），插入期间的索引维护（分裂、
嵌套、前缀压缩），系统 catalog/ACE 更新，usage map，数据库/表/列/索引属性的解析与
持久化（`PropertyMap`/`PropertyMaps.save` 把 blob 写回所属 catalog 行的 `LvProp`，
包括数据库创建时间属性、表 builder 的创建时表/列属性、属性更新/删除，以及 Java
builder 的 `HashMap` 属性顺序），以及表关系（`RelationshipBuilder.toRelationship`
写入 `MSysRelationships` 行、关系 catalog 对象和 ACE，以及带 `ForeignKeyReference`
元数据的支撑外键索引；`Database.getRelationships` 读回它们），外键**强制**
（`FKEnforcer`：参与引用完整性关系的表中每一行添加、更新或删除时，都会通过索引键查找
与另一侧核对；级联更新把父键变化传播到子行；级联删除删除子行（或把其外键置 null）；
违规操作抛出携带 Java `ConstraintViolationException` 消息的 `ConstraintViolation`，并
保持文件不变；`Database.setEnforceForeignKeys`，默认开启，与 jackcess 相同），链接表
**创建**（`Database.createLinkedTable`：合成对象 id、带 `Database`/`ForeignName` 值的
类型 6 catalog 行和 ACE），空闲页处理（`Database.setReuseFreePages` 重新分配已释放页，
`Database.reclaimFreeSpace` 截断末尾空闲页），显式自动编号插入
（`Database.setAllowAutoNumberInsert`、按表覆盖和 LONG 的 `handleInsert`/水位线记账），
复杂列（`ColumnBuilder.setMultiValue`/`setValueList`/`setRowSource`/`setAttachment`：
`MSysComplexColumns` 行、带父外键/顺序/值列的按列平面表、其索引和 usage map、
`COMPLEX_TYPE` 定义和复杂 id、表的 `0x00040000` catalog 标志、作为复杂外键 id 的行值、
复杂自动编号计数器、带复杂列的 `ColumnBuilder.addToTable`，以及多值和附件的
`ComplexValueForeignKey` 读写 API），以及版本历史（只追加 memo）列
（`ColumnBuilder.setAppendOnly` 作用于 memo 列：带 memo 列及其 `Modified_...` 日期列的
按列 `MSysComplexTypeVH_<guid>` 类型表、添加的 `COMPLEX_TYPE` 列上的 `VERSION_HISTORY`
ext 标志、memo 列及其表上的 `AppendOnly` 属性、按外键 / memo 值 / 修改日期 / 自动编号
key 布局的平面表、`ColumnBuilder.addToTable` 路径，以及通过
`Database.version_history_info` 读回列和日期，外加 `getVersions`/`addVersion` API；其
强类型更新/删除方法像 jackcess 一样被拒绝）。对支持计算列的格式（V2010 及以后）移植了
计算列：计算定义标志和长度规则、`ResultType`/`Expression` 属性、通过表达式引擎在添加
和更新时按依赖顺序求值、按结果类型包装的值编码及读回解包，`calc`/`calcupdate` 在
V2010/V2016/V2019 中逐字节一致。

列默认值和属性驱动的列校验器已实现：插入时如果行值为 null，会按 Java 库使用的相同
表达式结果类型求值并强制转换非自动编号列的 `DefaultValue` 属性
（`ColumnImpl.generateDefaultValue`）；`Required`（默认 false）和 `AllowZeroLength`
（默认 true，仅文本列）属性在插入和更新时按 Java `InvalidValueException` 消息强制执行。
支持的 `DefaultValue` 形式是能够逐字节复现的那些：

- 数值和 `BOOLEAN` 列的数值字面量（`0`、`-1`、`1.5`、科学计数法、
  `TRUE`/`FALSE`/`YES`/`NO`/`ON`/`OFF`/`NULL`），包括 Java 表达式引擎的转换
  （`LONG` 列对小数按银行家舍入，布尔值接受任意非零数字，`BigInt` 截断），
- `TEXT`/`MEMO` 的带引号文本字面量（`"abc"`，支持双引号转义，以及单引号），
- `TEXT`/`MEMO` 的不带引号文本默认值，Java 库将其视为字面文本（整个去除首尾空白的
  属性），除非它以 `=` 开头。

以 `=` 开头的默认值在 Java 库中属于完整表达式模式；本移植返回明确的 `Unsupported`
错误，而不是写入不同的值。`#date#` 字面量、日期/GUID/二进制列上的默认值、以 `#` 或
`[` 开头的文本列默认值、对本移植精确小数解析器来说过大的数值字面量，以及 jackcess 必须
舍入的 `NUMERIC`/`MONEY` 字面量（`NUMERIC` 列的 scale，或货币值的四位小数，其精确
二进制 `double` 需要更多位，如 `0.1`）也都报告为不支持，而不是近似处理。jackcess 能
精确存储的 `NUMERIC`/`MONEY` 字面量（`1.5`、`2.25`、……）受支持。与 jackcess 一样，
更新从不生成默认值（null 保持 null，但受校验器约束），属性保存会重新加载缓存表的默认值
和校验器。

链接数据库的**读取和写入**已移植（`util.LinkResolver`：`DEFAULT` 拒绝和
`UNRESTRICTED`；linkee 数据库是 linker 拥有的子对象，链接 catalog 行通过
`get_table`/`find_table`/`read_rows_with_ids` 解析，暴露
`isLinkedTable`/`getLinkedDatabases`，所有表名写入入口（行、schema、复杂/版本历史值）
都委托给 linkee，因此 linker 文件不变而 linkee 被修改）。每种文件类型的附件负载都逐
字节一致：Access 以原始形式存储的文件类型保持原始，其他所有扩展名都用忠实的 zlib
1.3.1 level-3 移植做 deflate 压缩（341/341 个 oracle 流逐字节一致），其 inflater 也能
读取压缩附件。表达式引擎（`impl.expr.Expressionator`：tokenizer、parser、值模型、
运算符、`LIKE`、日期、转换和大量内建函数子集）在 `src/expr.rs` 中独立移植，并针对
Jackcess 自身的计算列求值用 434 个 oracle 用例验证；它是计算列、`=...` 默认值和校验
规则的基础。公共游标 API 已移植（`Cursor`、`TableScanCursor`、`IndexCursor`：按 map、
列或条目 find/first/next/closest、迭代、当前行 get/set、更新和删除），包括行范围
（`setStartRow`/`setEndRow`/条目边界、包含标志、边界位置）、保存点和
`IterableBuilder`/`EntryIterableBuilder` 迭代器。分隔文本导入/导出工具
（`ExportUtil`/`ImportUtil`：选项、过滤器、引号、header、建表，以及带目录/流输出、
文件名转义、覆盖/追加的 `exportAll`）已移植并与 Java 比较。表、查询和列的显示名 map
（`NameMap`/`TableNameMap`/`QueryNameMap`，二进制 blob 格式、记录和不匹配项）从所属
属性解析，并通过 `Table` 暴露，与 Java 的 dump 及畸形 blob 错误一致。数据库行为设置
在影响写出字节时已移植：`setEvaluateExpressions`（像 Java 一样控制计算列、默认值和
表达式/行校验器，同时 `Required`/`AllowZeroLength` 保持生效）、固定偏移时区
（UTC/GMT-05:00 及任意偏移）用于所有日期转换和 catalog 时间戳，以及存储的
`ColumnOrder`/`DateTimeType` 设置。基于表达式的字段和表校验规则
（`ValidationRule`/`ValidationText`）在插入和更新时按精确 Java 消息强制执行，拒绝时
不改变文件和自动编号计数器；表达式引擎现在还覆盖日期/时间、`Format`、财务函数、
`Rnd`/`GenGUID` 以及 Jackcess 实现的其他内建函数，包括自定义 `Format` 模式
（3280 个 oracle 用例）。GUID 自动编号列在插入时生成版本 4 UUID（遵守显式插入策略及
其错误，并在更新/`KEEP_VALUE` 时保留该值）。本 crate 现在为加密工作依赖
`rc4`/`sha1` crate（见下文）。Jet 4 数据库**编码**（"Encode/Decode Database"）和 Jet
数据库**密码**从 jackcess-encrypt 5.0.2 移植，并且在相同 key/密码下与其逐字节一致：
页编码是按 `encodingKey ^ pageNumber` 生成 key 的 RC4，page 0 不掩码并保存 key；密码
字段是创建日期掩码下的编码密码，ACE SID 被重新掩码（`SidRemasker`），使文件保持可打开。
加密/编码数据库可通过移植的 `CodecProvider` 读取
（`Database::open_path_with_password`、`open_path_with_provider`），文件级操作
（`crypt::encode_file`/`decode_file`、`PasswordUtil::set/remove_database_password`，
就地与路径形式）精确复现 Java 字节，包括错误密码拒绝（Jet 格式不存储密码 verifier，
因此两套实现都像 jackcess-encrypt 一样从存储字段检测）。ACE（`.accdb`）加密从同一
add-on 移植：agile `OFFICE_2010`/`OFFICE_2013`（AES-CBC、SHA-1/SHA-512、密码 key 派生
和 XML `EncryptionInfo` 描述符）支持读取**和**写入，另外 ECMA 标准、非标准 AES、
RC4 CryptoAPI 和 Office 二进制 RC4 支持读取和移除加密。与 jackcess-encrypt 一样，
加密的 `.accdb` 不是 OLE `EncryptedPackage` 容器：page 0 保持明文（编码 key、掩码密码
字段、`EncryptionInfo`），每一页都作为一个扇区整体加密，`IV = H(keyDataSalt ‖
(encodingKey XOR pageNumber))`，因此加密不会移动数据。确定性模式（固定
salt/key/verifier 和编码 key，对应 Java 公开的 `AgileEncryptionWriter` 构造函数）使
加密字节完全一致；默认随机模式则与 Java 交叉读取/解密来验证。查询定义通过读取
`MSysObjects` 类型 5 行并按对象 id 分组 `MSysQueries` 集成到 `Database`
（`get_queries`、`get_system_queries`、`get_query`、参数）中，读取侧元数据也已移植：
`TableMetaData`（LOCAL/LINKED/LINKED_ODBC、标志、创建/更新日期、链接数据库/表、连接名）、
表/列/索引元数据、行数、SQL 类型和按单位长度。Jet 3 / Access 97 的**读取**已移植
（2048 字节页、跳转表可变列行格式、Jet 3 索引、溢出行为、长值、windows-1252 代码页
文本、强制只读打开），并针对 27 个真实 V1997/V2000/V2003 文件验证；OLE 工具
（`OleBlob`/`OleUtil`：复合文件 header/FAT/mini-FAT/directory、流、简单包、链接、
复合存储）已移植，并在真实 `blobV2007.accdb` 以及合成/损坏 fixture 上验证。
`ErrorHandler`/`DebugErrorHandler` 的行错误语义已移植（坏列值被替换——Java 从不跳过
该行——而表级失败仍会暴露），支持数据库/表/游标覆盖以及 Java 的回调上下文/消息。查询
属性 map 和查询名称 map（`Query.getProperties`/`getNameMap`，从查询的 `LvProp` 读取）
已移植且可保存，`CustomLinkResolver`（map/loader 形式、临时数据库 loader、相对/绝对
路径）已移植用于移动后的链接数据库。Jet 3 **编码**数据库可读取并解码（在真实 V1997
文件上针对 Java 页变换验证），Jet 3 密码字段在读取时被遵守；公共编码/设置密码 API
保留 Java 的拒绝行为。索引写入经过大规模压力验证（深多级 B 树、分裂、churn、复合和
降序键），包括 Java 的索引页缓存 LRU 及其陈旧页尾行为。`DateTimeType` 按 jackcess 的
真实语义实现（DATE 读取为带 millis 视图的日期位，LOCAL_DATE_TIME 是默认值；5.0.4 没有
STRING 模式），两种模式写出的字节都逐字节一致。`ColumnOrder` 是加载列上的真实视图
（行/写入用数据顺序，`get_columns` 用显示顺序），内存数据库
（`MemFileChannel`/`MemDatabase`：创建/打开/取字节）写出的字节与磁盘文件相同。真实
Access/jackcess-encrypt 加密文件（编码的 Jet 3/4、数据库密码、agile/RC4/AES Office
文件）像 Java 一样精确读取和解密。MSISAM / Microsoft Money `.mny` 数据库可读取
（Jet 4 布局、旧的从文件派生和现代 MD5/SHA-1 密码 key、第 1-14 页编码），与 Java 精确
一致；全部 109 个 JDK 代码页（87 个单字节 + 22 个多字节，包括 IBM EBCDIC DBCS 状态机）
对 Jet 3 文本逐字节精确解码/编码，并用超过一千万次比较对 JVM 验证。`DateTimeType` 还
驱动属性 map 日期、catalog 创建/更新日期和索引搜索 key（`Value::DateBits` 相等性遵循
`java.util.Date`），损坏 null mask 的 `ErrorHandler` 路径像 Java 一样按列重试 mask
（428 行回调轨迹一致）。工具层一致性已完整覆盖 `Double.toString`/`Float.toString`
（包括次正规数和最短往返 ties）、从文本导入 Java 序列化二进制、`ColumnFormatter` 工具
（包括其内部属性保存），内存数据库现在直接通过 channel 打开，不需要任何临时文件。
表达式 `Format` 的数值路径使用任意精度小数（`bigdecimal`），因此极端 double 和超过
38 位的系数与 Java 的 `BigDecimal` 输出一致。数据库时区除固定偏移外，还通过
`chrono-tz` 支持 IANA 时区（DST、间隙/重叠和历史 LMT 偏移），所有日期路径（行、
catalog、属性、索引）都使用它。公共读取 API 已对照 Java 审计
（`get_file_format`、charset、zone、排序顺序、表名、元数据、索引/ACE 数据、游标），
18 个 dump 精确一致。

按参考实现属于范围之外（jackcess 5.0.4 / jackcess-encrypt 没有暴露 API 或写入器，因此
没有可移植或比较的内容）：创建后删除列或索引、删除关系
（`Database.deleteRelationship`）、`MSysAccessObjects`/VBA、JDBC/ODBC 远程数据访问、
MSISAM 和 Jet 3 的**写入**，以及写入侧的旧 Office 加密方案（读取和移除已移植）。这些
**未实现，也不计划实现**，不计为缺口。

剩余的一致性缺口（较小）：jackcess 自身无法读取的复杂列
（`UnsupportedColumnInfo`）会作为不支持的数据呈现（Jackcess 只暴露其元数据）；自定义
`ImportFilter` 提供的非文本值的 Java 对象序列化无法在 Rust 中复现（没有 Java 对象
模型）；如果 JVM 的 tzdata 版本与 `chrono-tz` 不同，历史 IANA 时区结果可能不同（已测试
矩阵通过）。两个已记录的 API 细节：`Value` 没有 `Hash`（通过公共 API 不可观察），以及
单独调用 `Table::set_date_time_type` 不会重新索引已加载的索引（应使用
`Database::set_date_time_type`，类似 Java 的按数据库工厂）。API 形态注意事项（重载
名称映射、`Table` 没有 database 反向引用、`Row` getter/迭代器返回 `Result`，以及
求值器尚未使用 `EvalConfig`）列在 Java API 兼容层一节中。
GUID 自动编号生成已实现（`UUID.randomUUID` v4 语义、显式插入策略、更新保留值）；生成
值按设计是随机的，因此以结构/行为方式验证，而显式 GUID 插入逐字节一致。
写入名称 map 仅适用于 Access（jackcess 只读取，本移植已实现读取）。
链接 ODBC 表（类型 4）仅在*元数据/定义*层面支持（catalog 行、列/索引，以及 Java
`TableDefinition has no data access` 拒绝）；其远程数据需要 JDBC/ODBC 驱动，而
jackcess 5.0.4 也不提供。没有链接查询或跨链接外键检查（关系是按数据库的，与 Access
自身的链接表行为一致）。剩余较小缺口：`StrConv` 的非美国 locale 和 Java 自身拒绝的
畸形 `Format` 模式（Jackcess 的 `Partition`/`D*` 聚合在 5.0.4 中也不存在），以及对
BINARY/OLE/GUID 值的规则会报告为不支持，而不是强制转换。Jet 3 与 jackcess 一样只读
（写入不支持）；其文本使用所有 JDK 代码页（87 个单字节 + 22 个多字节，包括 IBM
EBCDIC DBCS 页，已与 JVM 逐字节验证；真实 Jet 3 文件如果头部指定 EBCDIC 页，Java 也
无法打开，因此通过文档化的 charset override 验证字符集）。MSISAM 只读（jackcess 中也
没有创建/写入 API）。OLE v4 扇区复合文件用生成的、经 POI 验证的 fixture 验证（不存在
Windows 编写的 v4 样本）。`DateTimeType` 也适用于属性 map 日期和索引搜索 key（用
`DATE`/`LDT` 模式查找和属性 dump 验证）；`STRING` 在 jackcess 5.0.4 中不存在。
jackcess 5.0.4 本身无法在表达式内读取 BOOLEAN **输入**列（会抛出
`ClassCastException`），因此只测试了 BOOLEAN 结果类型；计算 `MONEY`/`FLOAT` 结果类型
受支持。记录一个诊断怪癖：对 Office 加密的 `.accdb` 调用
`Database.getDatabasePassword()` 会返回无意义的 page-0 key 材料（不是密码），在*随机*
key 下两套实现会在那里存储不同的 key 材料；harness 只在确定性模式下比较完整的加密
page 0，否则忽略那一行诊断输出。

对 jackcess 支持的格式（V2010 及以后；V2003/V2007 以 Java 消息拒绝）移植了计算列：
计算 ext 标志、`CALC_FIXED_FIELD_LEN`/`CALC_EXTRA_DATA_LEN` 长度规则、builder 顺序中的
`ResultType`/`Expression` 属性、通过表达式引擎在添加和更新时按依赖顺序求值、按类型的
包装值编码（通用定长、布尔、文本、经长值路径的 memo、numeric）以及读回解包。只追加/
版本历史 memo 列**已**移植，包括 `ColumnBuilder.addToTable`（添加 memo 列，创建
jackcess 在其旁边添加的版本历史复杂列及其支撑表，现有行获得接下来的复杂 id，表的
`AppendOnly` 属性在列属性之后保存），复杂多值和附件列也已移植（先创建支撑表和
`MSysComplexColumns` 行，标记表的 catalog 标志，列写入之前已有的行获得接下来的复杂
id）。本移植拒绝不支持的部分（`Unsupported`/`IllegalArgument`），而不是半实现它们。

对**计算列**使用 `KEEP_VALUE` 会像 Java 库一样重新计算它：计算列不参与原始字节复用
（`_calcColEval.willCalculate`），保留的输入被重新计算的值替换；`calcupdate` 和计算列
单元测试固定了这一点。

已知的复杂列缺口：既不被 `MultiValueColumnInfo`、`AttachmentColumnInfo`，也不被
`VersionHistoryColumnInfo` 识别的复杂列（jackcess 的 `UnsupportedColumnInfo`）会被读作
`ComplexDataType::Unsupported`；附件 OLE 包装器只理解 jackcess 的两种存储类型（包装头、
内容头和文件扩展名都被精确写入/读取，原始负载按原始形式存储，其他文件类型用 level 3
deflate 压缩且 zlib 流逐字节一致，但不会构建"真正"的 OLE 复合文档——这与 jackcess 在
OLE 列中存储原始负载的行为一致）。

注意：jackcess 在删除行时不会删除该行的版本（没有级联），因此 `versionhistory` 场景中
被删除的行会在平面表中留下其版本行；本移植与此一致。同样，强类型的
`Version.update()`/`delete()` 和 `VersionHistoryColumnInfo.updateValue`/
`deleteValue`/`deleteAllValues` 在 jackcess 中抛出 `UnsupportedOperationException`，
本移植对相应方法返回 `Unsupported`。

注意：`src/property.rs` 实现属性 blob 的解析/序列化、对象/列/索引分组模型以及 `save()`
（通过 `Database::write_property_maps` 的 catalog 行机制）。属性保存会重写整个 catalog
行，因此同一行的其他*长值*列（例如链接表的 `Lv`、`LvModule` 或 `Connect`）会被解码并
重新写入，而不是像 `TableImpl` 的 `KEEP_VALUE` 路径那样保留原始字节；属性场景的行只
包含 `LvProp`，因此它们的字节匹配。`PropertyMap` 的值限于 `PropertyMapImpl` 能写出的
标量类型（见 `src/property.rs` 顶部的约定）；二进制/`OLE` 值必须是字节数组，
`NUMERIC` 值必须能放入 `i128`。

注意：外键强制器的级联更新/删除会从子行的列*值*重写子行（读回并重新编码），而不是像
`FKEnforcer` 那样对未修改列传入 `Column.KEEP_VALUE`。对于定长列和普通变长值，重编码
逐字节一致（`fkadd`/`fkdel`/`fkupd` 场景没有长值），但对持有 MEMO/OLE 值的行做级联
更新会分配新的长值页，而不是保留现有页。级联触及的行集在修改之前收集，而不是通过
`IndexCursor` 实时迭代；对于这些场景覆盖的关系，行集是相同的。

`partialupdate` 表形态（三个索引，其中一个是在 TEXT 列上的唯一索引，165+ 行）以前在
ACE/V2016 上的差异**不是分配器 bug**：它是场景生成器调用粒度的差异，现已修复。
`Gen.java` 用 `Table.addRowFromMap` 添加每一行，因此 jackcess 对每一行都执行完整的
逐行写入：`PageChannel.startWrite`、数据页写入，然后
`updateTableDefinition` -> `IndexData.update` -> `IndexPageCache.write`
（`handleEmptyPages`、`preparePagesForWriting` 及其条目前缀压缩/`splitDataPage`/
`allocateNewPage`，然后 `writeDataPages`）。`mdbgen` 曾对整个行列表调用一次
`Database::add_rows`；那是 jackcess **批量** `TableImpl.addRows` 的移植，它把索引改动
保存在内存中，只在批次结束时写入/分裂索引页一次。两条路径都是忠实的（在采样到 500 行
时，批量对批量、逐行对逐行在 V2003、V2007 和 V2016 上逐字节一致），但一旦索引页越过
`maxPageEntrySize`，它们就是不同的历史。对于 V2016（`GENERAL` 排序顺序和 general
index codes），唯一的 `ByName` 页大约在 150 行 `fillerNNN` 时首次越过该阈值：逐行路径
在每行之后于 `preparePagesForWriting` 中重新计算压缩条目前缀，因此该页仍然放得下；而
批量路径只在结束时压缩，然后分裂。500 行时，逐行文件的 `ByName` 根位于第 92 页，
叶子页为 102/103/107，数据页为 94、96-101、104-106（442,368 字节）；批量文件的叶子页
为 105/106，数据页为 94、96-104（438,272 字节）；索引 usage map（第 95 页）、全局
usage map（第 1 页）、主键条目的行 id 以及第一次分裂之后的每一页都不同。一个相关效应
是 jackcess 在读写时复用索引页缓冲区（`IndexData._indexBufferH`），因此页面的空闲空间
中会保留上一次更大写入的字节：165 行时，逐行 page 92 和批量 page 92 具有相同的
header、条目和空闲空间，只在已用区域之后的 1,557 个空闲空间字节上不同（批量写入在那里
留下零）。V2003（Jet 4 index codes）和 V2007（`GENERAL_LEGACY`）在更大的行数（约 300）
才越过相同阈值，这就是同一张表在 165 行时看起来逐字节一致的原因。

`mdbgen` 现在每次只用单行调用添加一行（`add_rows_one_by_one`），对应 `Gen.java` 的
`addRowFromMap` 循环，因此场景在任意行数下都逐字节一致；`Database::add_rows` 保留批量
`TableImpl.addRows` 语义（文档中已说明，包括何时需要逐行历史）。`diag200` 场景
（`partialupdate` 表形态加 200 行逐行添加的 filler 行）属于 harness 默认场景，并固定了
这一点。




