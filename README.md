<h1 style="font-size: 48px;">别倒卖了😭</h1>

# NekoSportsWorldTool

![Release](https://img.shields.io/github/v/release/YanamiNeko/NekoSportsWorldTool)
![Build](https://github.com/YanamiNeko/NekoSportsWorldTool/actions/workflows/release.yml/badge.svg)
![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-blue)

运动世界校园版全协议自动化工具，纯 Rust 单文件，GUI / CLI 双模式。

> 📌 本仓库是 [YanamiNeko/NekoSportsWorldTool](https://github.com/YanamiNeko/NekoSportsWorldTool) 的**衍生版本**：
> 在上游基础上新增「模拟位置」功能（见下文），其余协议实现与上游一致。
> 原项目采用 CC BY-NC 4.0（禁止商用），本衍生版本沿用同一许可，版权归原作者所有。

> ⚠️ 仅供学习研究，请勿用于违反学校规定的场景，后果自负。

## 功能

- **跑步**：一键全链（策略 → 打卡点 → 轨迹 → 提交 → OBS → 验证），轨迹按打卡点自然生成，开始时间支持随机或指定到前 3 天
- **模拟位置**：自己设定本次跑步的定位锚点（决定去哪个学校拉打卡点）。支持 WGS84 / GCJ-02 / BD-09 三种坐标基准自动换算、粘贴手机地图坐标（自动纠正「经度,纬度」顺序）、地图点选取点、命名收藏一键切换；启用后覆盖设备信息页的锚点，且跑步流程不再自动漂移它
- **AI 运动**：任务 / 自由练习双模式，批量补签（前 60 天 × 多项目）
- **数据**：学期完成度、违规自查、排行榜（个人/班级/院系 × 日/月、室内榜、历史榜）、个人主页
- **登录**：支持人机验证自动通过
- **自动更新**：桌面端检查/下载/自替换并重启（GitHub Release 直连，无镜像）；Android 下载 APK 调起系统安装器；启动检查模式可选（静默/询问/关闭）

## 界面

![GUI](docs/screenshot.png)

## 技术特点

- 完整实现 NetSecKit 信封加密与响应解密，每个响应强制 RSA 验签（fail-closed）
- 人机验证全程本地完成，不依赖任何外部服务
- 轨迹生成器：打卡点拟合环 + 自然速度曲线 + 哨兵点 / 断崖 / 点位吸附
- 设备身份持久化（DeviceId / 安装时间 / MAC），稳定不易触发风控
- GUI 与 CLI 共用同一套协议层，数据文件互通

## 使用

双击 exe 进入 GUI；CLI 用法：

```text
NekoSportsWorldTool login --user <手机号> --pass <密码> --remember
NekoSportsWorldTool run                                  # 一键跑步（--altitude 17.2 固定海拔，或 11.6-22.8 映射到区间）
NekoSportsWorldTool location set --lat 38.901678 --lon 121.540241 --datum wgs84 --city 大连市
NekoSportsWorldTool location                             # 查看当前模拟位置（三套基准换算）与收藏
NekoSportsWorldTool ai --sport 2 --score 26000           # AI 运动
NekoSportsWorldTool rank main --type 1 --sort 1          # 排行榜
NekoSportsWorldTool update [--check]           # 自动更新（--check 仅检查不下载）
NekoSportsWorldTool template --file run.gpx   # 本地分析真实记录海拔（不会上传）
NekoSportsWorldTool help                                 # 全部命令
```

### 模拟位置

GUI 在「模拟位置」页；CLI 用 `location` 子命令，两边读写同一份 `config.json`。

- **它是干什么的**：定位锚点决定向服务端要哪一批打卡点（也就是「在哪个学校跑」）。启用模拟位置后，跑步改用这个坐标，不再依赖设备信息页的 `anchor_lat/anchor_lon`，也不会在跑完后按打卡点随机漂移（漂移会把用户设的点改掉）。
- **坐标基准别选错**：手机地图 App 里的经纬度多为 **WGS84**；高德/腾讯是国内加密的 **GCJ-02**；百度是 **BD-09**（服务端与轨迹的基准）。选错会偏 300~600 米，很可能一个打卡点都拉不到。
- **粘贴识别**：直接粘 `38.901678,121.540241`，程序会顺手纠正「经度在前」的顺序。
- **地图取点**：页面上的黄点是已缓存的打卡点（上次拉取结果），单击地图即可把模拟位置移过去；「对齐打卡点中心」取这些点的质心，「用设备锚点填充」复制设备信息页的坐标。
- **城市**：会原样写进提交记录的 `address`；留空则沿用设备信息页的城市。异地跑记得填，否则地址与轨迹对不上。
- **推荐流程（第一次设位置）**：① 从地图 App 抄学校中心坐标，选对基准粘进来 → ② 点「重新读取点位」拉一次该锚点周围的打卡点（经典/道路模式都能拉，需已登录）→ ③ 点「对齐打卡点中心」把锚点放到这簇点位的质心 → ④「应用并保存」。锚点越接近打卡点质心，越不容易出现「点位为空」。
- **轨迹形状不由你画**：不画手绘路线时，实际跑出来的形状取决于学校那批打卡点的分布（经典模式拟合过全部打卡点的闭合环，道路模式再贴合 OSM 路网并规避建筑）；模拟位置只决定「去哪个学校拉点」。
- **手绘路线（可选）**：「模拟位置」页把地图切到「画路线」后，直接在地图上单击加点、拖顶点微调、右键删点，支持闭环/撤销/清空；底图默认用 Carto（OSM 数据，PNG），也可切 OSM 德国镜像 / OSM 官方 / 高德（GCJ-02，注意对方条款）。
  - 「串上打卡点」会把离路线超过 25 m 的打卡点插进最近的边——**必须做**，否则服务端可能判定「未经过打卡点」；
  - 「按绘制长度设距离」把跑步距离对齐到画的长度；保存后跑步页会优先按这条路线生成轨迹（记得距离/配速仍要在有效窗口内）；
  - 手绘路线存在 `config.json` 的 `drawn_route` 里，`run` 子命令也会自动使用它；换学校记得清空重画。
  - 只支持 **PNG** 瓦片（本机没有 JPEG 解码器），自定义 `tile_url` 时注意返回格式。

```text
location                         # 显示启用状态 + WGS84/GCJ-02/BD-09 三套坐标 + 收藏列表
location set --lat <纬度> --lon <经度> [--datum wgs84|gcj02|bd09] [--city 城市] [--label 备注]
location on | off                # 启用 / 停用（配置保留，随时切回）
location clear                   # 清除模拟位置（收藏保留）
location save <名称>             # 把当前生效的模拟位置存为收藏
location use <名称>              # 应用收藏
location rm <名称>               # 删除收藏
```

`template` 只读取用户手动选择的本地 GPX/JSON 文件，输出采样点、海拔范围、累计上升和累计下降，
不会登录、访问服务器或把模板记录接入跑步上传流程。

手动海拔支持三种状态：留空使用自动海拔；填写单个数字（例如 `17.2`）让所有轨迹点使用固定海拔；填写区间（例如 `11.6-22.8`）将生成的海拔曲线映射到该上下限内，同时保留轨迹的相对起伏。

青龙定时任务：首次手工 `login --remember` 一次，之后定时挂 `run` 即可。

## 常见问题

- **提示未登录 / 401**：先执行 `login --remember`（GUI 勾选记住密码等价），会话失效会自动重登
- **10121 设备风险**：设备身份是持久化的，删除 `identity.json` 等于换了新设备，不要频繁删
- **10603 点位限流**：点位接口 5 分钟限 3 次，程序内置 300s 缓存，正常使用不会触发
- **点位为空 / 一个打卡点都没有**：先检查模拟位置的坐标基准（WGS84/GCJ-02/BD-09 选错会偏 300~600 米），再看坐标是否落在学校范围内；点位接口只返回锚点附近的打卡点
- **榜单为空**：当天还没有人产生有效里程，查询会自动回退最近 3 天
- **检查更新失败**：更新仅直连 GitHub，网络不通时到 [Release](https://github.com/YanamiNeko/NekoSportsWorldTool/releases) 手动下载
- **手动更新方式**：Windows 下载新的 `nekosportsworldtool.exe` 后退出软件，用新文件替换旧文件；请保留 `config.json`、`identity.json`、`points_cache.json` 等配置文件，不要删除或修改。
- **人机验证失败**：自动重试 3 轮，仍失败大概率是网络波动，稍后再试
- **登录报 `invalid peer certificate: UnknownIssuer`**：程序必须验得过站点证书链。启动后日志页第一行会打印 `TLS 信任源：系统 N 张 + 内置 M 张 + 额外 K 张`：
  - `系统 0 张（系统库读取失败…）` → 本机证书库读不到（受限权限/精简系统），程序已自动用内置 Mozilla 根兜底，一般仍能登录；
  - 数量正常却仍握手失败 → 网络里有 TLS 中间人（校园网 / 公司代理 / 杀软 HTTPS 扫描），把它的根证书导出成 PEM，设环境变量 `NEKO_CA_FILE=<路径>` 后重启程序（多个文件用 `;` 分隔）；
  - 自检命令：`nekosportsworldtool tls`（打印信任源 + 对 Geetest 域名做一次握手），`--url` 可换目标。
- **杀软误报**：未签名编译产物可能被误报，自行判断后加白

## 构建

```bash
cargo build --release                        # GUI + CLI
cargo build --release --no-default-features  # 仅 CLI
```

推 tag 自动构建 Windows / macOS / Linux 四平台产物并发布 Release（见 `.github/workflows/release.yml`）。

## 许可证

[CC BY-NC 4.0](LICENSE) © YanamiNeko（禁止商用）
