//! Windows 2000 SP4 简中的时区表（`I386/HIVESFT.INF` 的 `Time Zones`，按 Index 排序）：显示名、标准/夏季时间名、
//! 偏移与夏令时规则。规则字段对应 TZI 的 SYSTEMTIME（月、第几个星期、星期几、时、分），用于生成 POSIX TZ 尾部规则。
//! 由 HIVESFT.INF 机械转换生成；同偏移下缩写重复的后一项取首词前两个字母以保持唯一。

/// 夏令时切换：每年第 `month` 月的第 `week` 个（5 为最后一个）星期 `weekday`（0 为周日）的本地 `hour:minute`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transition {
    pub month: u8,
    pub week: u8,
    pub weekday: u8,
    pub hour: u8,
    pub minute: u8,
}

/// 一个 Windows 时区。
pub struct Zone {
    /// 组合框中的显示名。
    pub display: &'static str,
    /// 标准时间名（“当前时区”文字）。
    pub standard: &'static str,
    /// 夏季时间名。
    pub daylight: &'static str,
    /// UTC = 本地时间 + bias（分钟，西为正）。
    pub bias: i32,
    /// 夏令时期间额外的 bias（通常 -60）。
    pub daylight_bias: i32,
    /// 夏令时开始与结束；没有夏令时为 None。
    pub rule: Option<(Transition, Transition)>,
    /// 原版 MapID 的海洋、陆地区域下标；-1 转为 None，同偏移时区可能共享区域。
    pub map_regions: [Option<u8>; 2],
    /// 标准/夏令时缩写：注册表键名各词首字母（Standard 在夏令时缩写中换成 D），与偏移一起唯一标识时区。
    pub abbreviation: (&'static str, &'static str),
}

#[rustfmt::skip]
pub const ZONES: [Zone; 74] = [
    Zone { display: "(GMT-12:00) 埃尼威托克，夸贾林岛", standard: "日界线标准时间", daylight: "日界线夏季时间", bias: 720, daylight_bias: -60, rule: None, map_regions: [Some(24), Some(25)], abbreviation: ("DST", "DDT"), },
    Zone { display: "(GMT-11:00) 中途岛，萨摩亚群岛", standard: "萨摩亚群岛标准时间", daylight: "萨摩亚群岛夏季时间", bias: 660, daylight_bias: -60, rule: None, map_regions: [Some(26), Some(27)], abbreviation: ("SST", "SDT"), },
    Zone { display: "(GMT-10:00) 夏威夷", standard: "夏威夷标准时间", daylight: "夏威夷夏季时间", bias: 600, daylight_bias: -60, rule: None, map_regions: [Some(28), Some(29)], abbreviation: ("HST", "HDT"), },
    Zone { display: "(GMT-09:00) 阿拉斯加", standard: "阿拉斯加标准时间", daylight: "阿拉斯加夏季时间", bias: 540, daylight_bias: -60, rule: Some((Transition { month: 4, week: 1, weekday: 0, hour: 2, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 2, minute: 0, },)), map_regions: [Some(30), Some(31)], abbreviation: ("AST", "ADT"), },
    Zone { display: "(GMT-08:00) 太平洋时间(美国和加拿大)；蒂华纳", standard: "太平洋标准时间", daylight: "太平洋夏季时间", bias: 480, daylight_bias: -60, rule: Some((Transition { month: 4, week: 1, weekday: 0, hour: 2, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 2, minute: 0, },)), map_regions: [Some(32), Some(33)], abbreviation: ("PST", "PDT"), },
    Zone { display: "(GMT-07:00) 山地时间(美国和加拿大)", standard: "山地标准时间", daylight: "山地夏季时间", bias: 420, daylight_bias: -60, rule: Some((Transition { month: 4, week: 1, weekday: 0, hour: 2, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 2, minute: 0, },)), map_regions: [Some(34), Some(35)], abbreviation: ("MST", "MDT"), },
    Zone { display: "(GMT-07:00) 亚利桑那", standard: "美国山地标准时间", daylight: "美国山地夏季时间", bias: 420, daylight_bias: -60, rule: None, map_regions: [None, Some(87)], abbreviation: ("UMST", "UMDT"), },
    Zone { display: "(GMT-06:00) 中部时间(美国和加拿大)", standard: "中部标准时间", daylight: "中部夏季时间", bias: 360, daylight_bias: -60, rule: Some((Transition { month: 4, week: 1, weekday: 0, hour: 2, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 2, minute: 0, },)), map_regions: [Some(36), Some(37)], abbreviation: ("CST", "CDT"), },
    Zone { display: "(GMT-06:00) 萨斯喀彻温", standard: "加拿大中部标准时间", daylight: "加拿大中部夏季时间", bias: 360, daylight_bias: -60, rule: None, map_regions: [None, Some(86)], abbreviation: ("CCST", "CCDT"), },
    Zone { display: "(GMT-06:00) 墨西哥城", standard: "墨西哥标准时间", daylight: "墨西哥夏季时间", bias: 360, daylight_bias: -60, rule: Some((Transition { month: 4, week: 1, weekday: 0, hour: 2, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 2, minute: 0, },)), map_regions: [None, Some(85)], abbreviation: ("MST", "MDT"), },
    Zone { display: "(GMT-06:00) 中美洲", standard: "中美洲标准时间", daylight: "中美洲夏季时间", bias: 360, daylight_bias: -60, rule: None, map_regions: [None, Some(85)], abbreviation: ("CAST", "CADT"), },
    Zone { display: "(GMT-05:00) 东部时间(美国和加拿大)", standard: "东部标准时间", daylight: "东部夏季时间", bias: 300, daylight_bias: -60, rule: Some((Transition { month: 4, week: 1, weekday: 0, hour: 2, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 2, minute: 0, },)), map_regions: [Some(38), Some(39)], abbreviation: ("EST", "EDT"), },
    Zone { display: "(GMT-05:00) 印地安那(东)", standard: "美国东部标准时间", daylight: "美国东部夏季时间", bias: 300, daylight_bias: -60, rule: None, map_regions: [None, Some(84)], abbreviation: ("UEST", "UEDT"), },
    Zone { display: "(GMT-05:00) 波哥大，利马", standard: "南美州太平洋标准时间", daylight: "南美州太平洋夏季时间", bias: 300, daylight_bias: -60, rule: None, map_regions: [None, Some(83)], abbreviation: ("SPST", "SPDT"), },
    Zone { display: "(GMT-04:00) 大西洋时间(加拿大)", standard: "大西洋标准时间", daylight: "大西洋夏季时间", bias: 240, daylight_bias: -60, rule: Some((Transition { month: 4, week: 1, weekday: 0, hour: 2, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 2, minute: 0, },)), map_regions: [Some(40), Some(41)], abbreviation: ("AST", "ADT"), },
    Zone { display: "(GMT-04:00) 加拉加斯，拉巴斯", standard: "南美州西部标准时间", daylight: "南美州西部夏季时间", bias: 240, daylight_bias: -60, rule: None, map_regions: [None, Some(82)], abbreviation: ("SWST", "SWDT"), },
    Zone { display: "(GMT-04:00) 圣地亚哥", standard: "南美州太平洋标准时间", daylight: "南美州太平洋夏季时间", bias: 240, daylight_bias: -60, rule: Some((Transition { month: 10, week: 2, weekday: 6, hour: 0, minute: 0, }, Transition { month: 3, week: 2, weekday: 6, hour: 0, minute: 0, },)), map_regions: [None, Some(82)], abbreviation: ("PSST", "PSDT"), },
    Zone { display: "(GMT-03:30) 纽芬兰", standard: "纽芬兰标准时间", daylight: "纽芬兰夏季时间", bias: 210, daylight_bias: -60, rule: Some((Transition { month: 4, week: 1, weekday: 0, hour: 2, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 2, minute: 0, },)), map_regions: [None, Some(81)], abbreviation: ("NST", "NDT"), },
    Zone { display: "(GMT-03:00) 巴西利亚", standard: "南美州东部标准时间", daylight: "南美州东部夏季时间", bias: 180, daylight_bias: -60, rule: Some((Transition { month: 10, week: 3, weekday: 0, hour: 2, minute: 0, }, Transition { month: 2, week: 2, weekday: 0, hour: 2, minute: 0, },)), map_regions: [None, Some(80)], abbreviation: ("ESAST", "ESADT"), },
    Zone { display: "(GMT-03:00) 布宜诺斯艾利斯，乔治敦", standard: "南美州东部标准时间", daylight: "南美州东部夏季时间", bias: 180, daylight_bias: -60, rule: None, map_regions: [Some(42), Some(43)], abbreviation: ("SEST", "SEDT"), },
    Zone { display: "(GMT-03:00) 格陵兰", standard: "格陵兰东部标准时间", daylight: "格陵兰东部夏季时间", bias: 180, daylight_bias: -60, rule: Some((Transition { month: 4, week: 1, weekday: 0, hour: 2, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 2, minute: 0, },)), map_regions: [None, Some(80)], abbreviation: ("GST", "GDT"), },
    Zone { display: "(GMT-02:00) 中大西洋", standard: "中大西洋标准时间", daylight: "中大西洋夏季时间", bias: 120, daylight_bias: -60, rule: Some((Transition { month: 3, week: 5, weekday: 0, hour: 2, minute: 0, }, Transition { month: 9, week: 5, weekday: 0, hour: 2, minute: 0, },)), map_regions: [Some(44), Some(45)], abbreviation: ("MAST", "MADT"), },
    Zone { display: "(GMT-01:00) 亚速尔群岛", standard: "亚速尔群岛标准时间", daylight: "亚速尔群岛夏季时间", bias: 60, daylight_bias: -60, rule: Some((Transition { month: 3, week: 5, weekday: 0, hour: 2, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 3, minute: 0, },)), map_regions: [Some(46), Some(47)], abbreviation: ("AST", "ADT"), },
    Zone { display: "(GMT-01:00) 佛得角群岛", standard: "佛得角群岛标准时间", daylight: "佛得角群岛夏季时间", bias: 60, daylight_bias: -60, rule: None, map_regions: [Some(46), Some(47)], abbreviation: ("CVST", "CVDT"), },
    Zone { display: "(GMT) 格林威治平时; 都柏林, 爱丁堡, 伦敦, 里斯本", standard: "格林威治平时标准时间", daylight: "格林威治平时夏季时间", bias: 0, daylight_bias: -60, rule: Some((Transition { month: 3, week: 5, weekday: 0, hour: 1, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 2, minute: 0, },)), map_regions: [Some(0), Some(1)], abbreviation: ("GST", "GDT"), },
    Zone { display: "(GMT) 蒙罗维亚，卡萨布兰卡", standard: "格林威治标准时间", daylight: "格林威治夏季时间", bias: 0, daylight_bias: -60, rule: None, map_regions: [Some(88), Some(89)], abbreviation: ("GRST", "GRDT"), },
    Zone { display: "(GMT+01:00) 贝尔格莱德，布拉迪斯拉发，布达佩斯，卢布尔雅那，布拉格", standard: "中欧标准时间", daylight: "中欧夏季时间", bias: -60, daylight_bias: -60, rule: Some((Transition { month: 3, week: 5, weekday: 0, hour: 2, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 3, minute: 0, },)), map_regions: [None, Some(66)], abbreviation: ("CEST", "CEDT"), },
    Zone { display: "(GMT+01:00) 萨拉热窝，斯科普里，维尔纽斯，索非亚，华沙，萨格勒布", standard: "中欧标准时间", daylight: "中欧夏季时间", bias: -60, daylight_bias: -60, rule: Some((Transition { month: 3, week: 5, weekday: 0, hour: 2, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 3, minute: 0, },)), map_regions: [Some(2), Some(3)], abbreviation: ("CEEST", "CEEDT"), },
    Zone { display: "(GMT+01:00) 布鲁塞尔，哥本哈根，马德里，巴黎", standard: "罗马标准时间", daylight: "罗马夏季时间", bias: -60, daylight_bias: -60, rule: Some((Transition { month: 3, week: 5, weekday: 0, hour: 2, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 3, minute: 0, },)), map_regions: [None, Some(64)], abbreviation: ("RST", "RDT"), },
    Zone { display: "(GMT+01:00) 阿姆斯特丹，柏林，伯尔尼，罗马，斯德哥尔摩，维也纳", standard: "西欧标准时间", daylight: "西欧夏季时间", bias: -60, daylight_bias: -60, rule: Some((Transition { month: 3, week: 5, weekday: 0, hour: 2, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 3, minute: 0, },)), map_regions: [Some(2), Some(3)], abbreviation: ("WEST", "WEDT"), },
    Zone { display: "(GMT+01:00) 中非西部", standard: "中非西部标准时间", daylight: "中非西部夏季时间", bias: -60, daylight_bias: -60, rule: None, map_regions: [None, Some(66)], abbreviation: ("WCAST", "WCADT"), },
    Zone { display: "(GMT+02:00) 布加勒斯特", standard: "东欧标准时间", daylight: "东欧夏季时间", bias: -120, daylight_bias: -60, rule: Some((Transition { month: 3, week: 5, weekday: 0, hour: 0, minute: 0, }, Transition { month: 9, week: 5, weekday: 0, hour: 1, minute: 0, },)), map_regions: [Some(4), Some(5)], abbreviation: ("EEST", "EEDT"), },
    Zone { display: "(GMT+02:00) 开罗", standard: "埃及标准时间", daylight: "埃及夏季时间", bias: -120, daylight_bias: -60, rule: Some((Transition { month: 5, week: 1, weekday: 5, hour: 2, minute: 0, }, Transition { month: 9, week: 5, weekday: 3, hour: 2, minute: 0, },)), map_regions: [Some(4), Some(68)], abbreviation: ("EST", "EDT"), },
    Zone { display: "(GMT+02:00) 赫尔辛基，里加，塔林", standard: "FLE 标准时间", daylight: "FLE 夏季时间", bias: -120, daylight_bias: -60, rule: Some((Transition { month: 3, week: 5, weekday: 0, hour: 3, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 4, minute: 0, },)), map_regions: [None, Some(67)], abbreviation: ("FST", "FDT"), },
    Zone { display: "(GMT+02:00) 雅典，伊斯坦布尔，明斯克", standard: "GTB 标准时间", daylight: "GTB 夏季时间", bias: -120, daylight_bias: -60, rule: Some((Transition { month: 3, week: 5, weekday: 0, hour: 2, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 3, minute: 0, },)), map_regions: [None, Some(67)], abbreviation: ("GST", "GDT"), },
    Zone { display: "(GMT+02:00) 耶路撒冷", standard: "耶路撒冷标准时间", daylight: "耶路撒冷夏季时间", bias: -120, daylight_bias: -60, rule: None, map_regions: [None, Some(70)], abbreviation: ("IST", "IDT"), },
    Zone { display: "(GMT+02:00) 哈拉雷，比勒陀利亚", standard: "南非标准时间", daylight: "南非夏季时间", bias: -120, daylight_bias: -60, rule: None, map_regions: [Some(4), Some(69)], abbreviation: ("SAST", "SADT"), },
    Zone { display: "(GMT+03:00) 莫斯科，圣彼得堡, 喀山,伏尔加格勒", standard: "俄国标准时间", daylight: "俄国夏季时间", bias: -180, daylight_bias: -60, rule: Some((Transition { month: 3, week: 5, weekday: 0, hour: 2, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 3, minute: 0, },)), map_regions: [Some(6), Some(7)], abbreviation: ("RST", "RDT"), },
    Zone { display: "(GMT+03:00) 科威特，利雅得", standard: "阿拉伯标准时间", daylight: "阿拉伯夏季时间", bias: -180, daylight_bias: -60, rule: None, map_regions: [None, Some(71)], abbreviation: ("AST", "ADT"), },
    Zone { display: "(GMT+03:00) 内罗毕", standard: "东非标准时间", daylight: "东非夏季时间", bias: -180, daylight_bias: -60, rule: None, map_regions: [None, Some(71)], abbreviation: ("EAST", "EADT"), },
    Zone { display: "(GMT+03:00) 巴格达", standard: "阿拉伯标准时间", daylight: "阿拉伯夏季时间", bias: -180, daylight_bias: -60, rule: Some((Transition { month: 4, week: 1, weekday: 0, hour: 3, minute: 0, }, Transition { month: 10, week: 1, weekday: 0, hour: 4, minute: 0, },)), map_regions: [None, Some(71)], abbreviation: ("ARST", "ARDT"), },
    Zone { display: "(GMT+03:30) 德黑兰", standard: "伊朗标准时间", daylight: "伊朗夏季时间", bias: -210, daylight_bias: -60, rule: Some((Transition { month: 3, week: 1, weekday: 0, hour: 2, minute: 0, }, Transition { month: 9, week: 4, weekday: 2, hour: 2, minute: 0, },)), map_regions: [None, Some(72)], abbreviation: ("IST", "IDT"), },
    Zone { display: "(GMT+04:00) 阿布扎比，马斯喀特", standard: "阿拉伯半岛标准时间", daylight: "阿拉伯半岛夏季时间", bias: -240, daylight_bias: -60, rule: None, map_regions: [Some(8), Some(9)], abbreviation: ("AST", "ADT"), },
    Zone { display: "(GMT+04:00) 巴库，第比利斯， 埃里温", standard: "高加索标准时间", daylight: "高加索夏季时间", bias: -240, daylight_bias: -60, rule: Some((Transition { month: 3, week: 5, weekday: 0, hour: 2, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 3, minute: 0, },)), map_regions: [Some(8), Some(9)], abbreviation: ("CST", "CDT"), },
    Zone { display: "(GMT+04:30) 喀布尔", standard: "阿富汗标准时间", daylight: "阿富汗夏季时间", bias: -270, daylight_bias: -60, rule: None, map_regions: [None, Some(73)], abbreviation: ("AST", "ADT"), },
    Zone { display: "(GMT+05:00) 叶卡捷琳堡", standard: "叶卡捷琳堡标准时间", daylight: "叶卡捷琳堡夏季时间", bias: -300, daylight_bias: -60, rule: Some((Transition { month: 3, week: 5, weekday: 0, hour: 2, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 3, minute: 0, },)), map_regions: [Some(10), Some(11)], abbreviation: ("EST", "EDT"), },
    Zone { display: "(GMT+05:00) 伊斯兰堡，卡拉奇，塔什干", standard: "西亚标准时间", daylight: "西亚夏季时间", bias: -300, daylight_bias: -60, rule: None, map_regions: [Some(10), Some(11)], abbreviation: ("WAST", "WADT"), },
    Zone { display: "(GMT+05:30) 加尔各答，马德拉斯，孟买，新德里", standard: "印度标准时间", daylight: "印度夏季时间", bias: -330, daylight_bias: -60, rule: None, map_regions: [None, Some(74)], abbreviation: ("IST", "IDT"), },
    Zone { display: "(GMT+05:45) 加德满都", standard: "尼泊尔标准时间", daylight: "尼泊尔夏季时间", bias: -345, daylight_bias: -60, rule: None, map_regions: [None, Some(74)], abbreviation: ("NST", "NDT"), },
    Zone { display: "(GMT+06:00) 阿斯塔纳，达卡", standard: "中亚标准时间", daylight: "中亚夏季时间", bias: -360, daylight_bias: -60, rule: None, map_regions: [Some(12), Some(13)], abbreviation: ("CAST", "CADT"), },
    Zone { display: "(GMT+06:00) 斯里哈亚华登尼普拉", standard: "斯里兰卡标准时间", daylight: "斯里兰卡夏季时间", bias: -360, daylight_bias: -60, rule: None, map_regions: [None, Some(74)], abbreviation: ("SLST", "SLDT"), },
    Zone { display: "(GMT+06:00) 阿拉木图，新西伯利亚", standard: "中亚北部标准时间", daylight: "中亚北部夏季时间", bias: -360, daylight_bias: -60, rule: Some((Transition { month: 3, week: 5, weekday: 0, hour: 2, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 3, minute: 0, },)), map_regions: [Some(12), Some(13)], abbreviation: ("NCAST", "NCADT"), },
    Zone { display: "(GMT+06:30) 仰光", standard: "缅甸标准时间", daylight: "缅甸夏季时间", bias: -390, daylight_bias: -60, rule: None, map_regions: [None, Some(74)], abbreviation: ("MST", "MDT"), },
    Zone { display: "(GMT+07:00) 曼谷，雅加达，河内", standard: "东南亚标准时间", daylight: "东南亚夏季时间", bias: -420, daylight_bias: -60, rule: None, map_regions: [Some(14), Some(15)], abbreviation: ("SAST", "SADT"), },
    Zone { display: "(GMT+07:00) 克拉斯诺亚尔斯克", standard: "北亚标准时间", daylight: "北亚夏季时间", bias: -420, daylight_bias: -60, rule: Some((Transition { month: 3, week: 5, weekday: 0, hour: 2, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 3, minute: 0, },)), map_regions: [Some(14), Some(15)], abbreviation: ("NAST", "NADT"), },
    Zone { display: "(GMT+08:00) 北京，重庆，香港特别行政区，乌鲁木齐", standard: "中国标准时间", daylight: "中国夏季时间", bias: -480, daylight_bias: -60, rule: None, map_regions: [None, Some(75)], abbreviation: ("CST", "CDT"), },
    Zone { display: "(GMT+08:00) 吉隆坡，新加坡", standard: "马来西亚半岛标准时间", daylight: "马来西亚半岛夏季时间", bias: -480, daylight_bias: -60, rule: None, map_regions: [Some(16), Some(17)], abbreviation: ("SST", "SDT"), },
    Zone { display: "(GMT+08:00) 台北", standard: "台北标准时间", daylight: "台北夏季时间", bias: -480, daylight_bias: -60, rule: None, map_regions: [Some(16), Some(17)], abbreviation: ("TST", "TDT"), },
    Zone { display: "(GMT+08:00) 珀斯", standard: "澳大利亚西部标准时间", daylight: "澳大利亚西部夏季时间", bias: -480, daylight_bias: -60, rule: None, map_regions: [Some(16), Some(17)], abbreviation: ("WAST", "WADT"), },
    Zone { display: "(GMT+08:00) 伊尔库茨克，乌兰巴托", standard: "北亚东部标准时间", daylight: "北亚东部夏季时间", bias: -480, daylight_bias: -60, rule: Some((Transition { month: 3, week: 5, weekday: 0, hour: 2, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 3, minute: 0, },)), map_regions: [Some(16), Some(17)], abbreviation: ("NAEST", "NAEDT"), },
    Zone { display: "(GMT+09:00) 汉城", standard: "韩国标准时间", daylight: "韩国夏季时间", bias: -540, daylight_bias: -60, rule: None, map_regions: [Some(18), Some(19)], abbreviation: ("KST", "KDT"), },
    Zone { display: "(GMT+09:00) 东京，大坂，札幌", standard: "东京标准时间", daylight: "东京夏季时间", bias: -540, daylight_bias: -60, rule: None, map_regions: [Some(18), Some(19)], abbreviation: ("TST", "TDT"), },
    Zone { display: "(GMT+09:00) 雅库茨克", standard: "雅库茨克标准时间", daylight: "雅库茨克夏季时间", bias: -540, daylight_bias: -60, rule: Some((Transition { month: 3, week: 5, weekday: 0, hour: 2, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 3, minute: 0, },)), map_regions: [Some(18), Some(19)], abbreviation: ("YST", "YDT"), },
    Zone { display: "(GMT+09:30) 达尔文", standard: "澳大利亚中部标准时间", daylight: "澳大利亚中部夏季时间", bias: -570, daylight_bias: -60, rule: None, map_regions: [None, Some(76)], abbreviation: ("ACST", "ACDT"), },
    Zone { display: "(GMT+09:30) 阿德莱德", standard: "澳大利亚中部标准时间", daylight: "澳大利亚中部夏季时间", bias: -570, daylight_bias: -60, rule: Some((Transition { month: 10, week: 5, weekday: 0, hour: 2, minute: 0, }, Transition { month: 3, week: 5, weekday: 0, hour: 3, minute: 0, },)), map_regions: [None, Some(76)], abbreviation: ("CAST", "CADT"), },
    Zone { display: "(GMT+10:00) 堪培拉，墨尔本，悉尼", standard: "澳大利亚东部标准时间", daylight: "澳大利亚东部夏季时间", bias: -600, daylight_bias: -60, rule: Some((Transition { month: 10, week: 5, weekday: 0, hour: 2, minute: 0, }, Transition { month: 3, week: 5, weekday: 0, hour: 3, minute: 0, },)), map_regions: [Some(20), Some(21)], abbreviation: ("AEST", "AEDT"), },
    Zone { display: "(GMT+10:00) 布里斯班", standard: "澳大利亚东部标准时间", daylight: "澳大利亚东部夏季时间", bias: -600, daylight_bias: -60, rule: None, map_regions: [Some(20), Some(21)], abbreviation: ("EAST", "EADT"), },
    Zone { display: "(GMT+10:00) 霍巴特", standard: "塔斯马尼亚岛标准时间", daylight: "塔斯马尼亚岛夏季时间", bias: -600, daylight_bias: -60, rule: Some((Transition { month: 10, week: 1, weekday: 0, hour: 2, minute: 0, }, Transition { month: 3, week: 5, weekday: 0, hour: 3, minute: 0, },)), map_regions: [Some(20), None], abbreviation: ("TST", "TDT"), },
    Zone { display: "(GMT+10:00) 符拉迪沃斯托克", standard: "符拉迪沃斯托克标准时间", daylight: "符拉迪沃斯托克夏季时间", bias: -600, daylight_bias: -60, rule: Some((Transition { month: 3, week: 5, weekday: 0, hour: 2, minute: 0, }, Transition { month: 10, week: 5, weekday: 0, hour: 3, minute: 0, },)), map_regions: [None, Some(77)], abbreviation: ("VST", "VDT"), },
    Zone { display: "(GMT+10:00) 关岛，莫尔兹比港，符拉迪沃斯托克", standard: "西太平洋标准时间", daylight: "西太平洋夏季时间", bias: -600, daylight_bias: -60, rule: None, map_regions: [None, Some(77)], abbreviation: ("WPST", "WPDT"), },
    Zone { display: "(GMT+11:00) 马加丹，索罗门群岛，新喀里多尼亚", standard: "太平洋中部标准时间", daylight: "太平洋中部夏季时间", bias: -660, daylight_bias: -60, rule: None, map_regions: [Some(22), Some(23)], abbreviation: ("CPST", "CPDT"), },
    Zone { display: "(GMT+12:00) 富士，堪察加半岛，马绍尔群岛", standard: "富士标准时间", daylight: "富士夏季时间", bias: -720, daylight_bias: -60, rule: None, map_regions: [Some(24), Some(25)], abbreviation: ("FST", "FDT"), },
    Zone { display: "(GMT+12:00) 惠灵顿，奥克兰", standard: "新西兰标准时间", daylight: "新西兰夏季时间", bias: -720, daylight_bias: -60, rule: Some((Transition { month: 10, week: 1, weekday: 0, hour: 2, minute: 0, }, Transition { month: 3, week: 3, weekday: 0, hour: 2, minute: 0, },)), map_regions: [Some(78), Some(79)], abbreviation: ("NZST", "NZDT"), },
    Zone { display: "(GMT+13:00) 努库阿洛法", standard: "汤加标准时间", daylight: "汤加夏季时间", bias: -780, daylight_bias: -60, rule: None, map_regions: [Some(78), Some(79)], abbreviation: ("TST", "TDT"), },
];
