================================================================
  Entelecheia Gateway — Auto-Installer USB Drive
================================================================

This USB drive was mounted automatically when you connected
the Entelecheia gateway via USB-C.

WHAT TO DO:

  Windows:   Open "install_evernight.bat" in the windows/ folder.
             Or accept the AutoRun prompt if one appears.

  Linux:     Run linux/install_evernight.sh

  macOS:     Double-click macos/install_evernight.command

  Android:   See android/install_evernight.txt
             (or just open http://10.0.99.1:8080 in a browser)

WHAT HAPPENS:

  1. The evernight client is installed as a background service.
  2. The USB-C NCM virtual network is configured.
  3. This machine registers as a node with the gateway.
  4. The gateway dashboard opens in your browser.

  The gateway will then auto-detect any industrial devices
  (PLCs, sensors) connected to its Ethernet ports and begin
  AI-assisted analysis.

SAFE TO REMOVE:

  After installation, you can disconnect the USB-C cable.
  The evernight service will reconnect automatically when the
  cable is plugged in again. For persistent network access,
  use the gateway's Ethernet or Wi-Fi uplink.

----------------------------------------------------------------
  Entelecheia 网关 — 自动安装程序
----------------------------------------------------------------

连接 USB-C 线后，本 U 盘会自动挂载。

安装方式：

  Windows:   打开 windows/ 文件夹中的 "install_evernight.bat"
  Linux:     运行 linux/install_evernight.sh
  macOS:     双击 macos/install_evernight.command
  Android:   查看 android/install_evernight.txt
             （或直接用浏览器访问 http://10.0.99.1:8080）

安装完成后，本机将作为节点注册到网关，evernight 客户端会
以后台服务方式自动运行。网关会自动识别连接到其网口的工业
设备并启动 AI 分析。

================================================================
