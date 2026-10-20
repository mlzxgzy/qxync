# qxync 的 Dolphin 角标插件（KOverlayIconPlugin）
#
#   cmake -S . -B build -DCMAKE_BUILD_TYPE=Release
#   cmake --build build
#   sudo cmake --install build --prefix /usr        # 系统级（推荐，Dolphin 直接加载）
#   cmake --install build --prefix ~/.local         # 用户级（需 QT_PLUGIN_PATH，见 README）
#
# 依赖：Qt6 Core、KF6 KIO（Arch: qt6-base kio cmake），见 README。

cmake_minimum_required(VERSION 3.16)
project(qxync_overlay VERSION 0.7.2 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_AUTOMOC ON)
include(GNUInstallDirs)

find_package(Qt6 REQUIRED COMPONENTS Core)
find_package(KF6KIO REQUIRED)

# Dolphin 从 <Qt 插件目录>/kf6/overlayicon 加载 KOverlayIconPlugin
# （Dolphin 源码 src/kitemviews/kfileitemmodelrolesupdater.cpp）。
add_library(qxync_overlay MODULE qxync_overlay.cpp)
target_link_libraries(qxync_overlay PRIVATE Qt6::Core KF6::KIOCore)

# 元数据改了要触发 moc 重新生成插件的 Q_PLUGIN_METADATA。
set_property(SOURCE qxync_overlay.cpp PROPERTY OBJECT_DEPENDS "${CMAKE_CURRENT_SOURCE_DIR}/qxync_overlay.json")

install(TARGETS qxync_overlay
        LIBRARY DESTINATION "${CMAKE_INSTALL_LIBDIR}/qt6/plugins/kf6/overlayicon")

# 角标图标：Dolphin 用 QIcon::fromTheme(<getOverlays 返回的名字>) 解析，
# 所以必须进 hicolor 主题（不能直接给路径）。
install(FILES
        icons/qxync-hydrated.svg
        icons/qxync-placeholder.svg
        icons/qxync-partial.svg
        DESTINATION "${CMAKE_INSTALL_DATADIR}/icons/hicolor/scalable/status")
