# typeface

共享系统字体布局与绘制库，产物 `libtypeface.so`。通过 Fontconfig 解析系统和用户字体，Pango 使用 HarfBuzz 与 FreeType
塑形、断行和度量，Cairo 同步绘制到调用者的裁剪 ARGB 缓冲。布局提供 UTF-8 字节索引到光标、命中和视觉选区的映射。
