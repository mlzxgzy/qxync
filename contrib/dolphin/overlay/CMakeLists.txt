# qxync 的 Dolphin 角标插件（KOverlayIconPlugin）
#
#   cmake -S . -B build -DCMAKE_BUILD_TYPE=Release
#   cmake --build build
#   sudo cmake --install build --prefix /usr        # 系统级（推荐，Dolphin 直接加载）
#   cmake --install build --prefix ~/.local         # 用户级（需 QT_PLUGIN_PATH，见 README）
#
# 依赖：Qt Core + KF KIO（Arch: qt6-base kio cmake；KF5 世代: qtbase5-dev
# libkf5kio-dev cmake），见 README。
#
# ★ Qt5/Qt6 双世代（M25 deb 分发）：Dolphin 从 <Qt 插件目录>/kf<Qt主版本>/overlayicon
#   加载本插件（源码里是 QStringLiteral("kf" QT_STRINGIFY(QT_VERSION_MAJOR)
#   + "/overlayicon")，21.12 之前是硬编码 "kf5/overlayicon"）。KF5 与 KF6 的
#   KOverlayIconPlugin 是同一个接口（KIOWidgets 头文件），插件源码双兼容；
#   但 .so 的 ABI 绑 KIO 大版本（libKF5KIOCore vs libKF6KIOCore），所以
#   deb 侧出 kf5/kf6 两个包、各自在对应世代的基线里编译。
#   默认找 Qt6（KF6）；-DQXYNC_KF_MAJOR=5 显式选 KF5。

cmake_minimum_required(VERSION 3.16)
project(qxync_overlay VERSION 0.7.2 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_AUTOMOC ON)
include(GNUInstallDirs)

# KF 大版本：5 或 6。缺省 6（Arch / 新发行版）；KF5 世代的打包脚本显式传 5。
set(QXYNC_KF_MAJOR "6" CACHE STRING "KF major version to build against (5 or 6)")
if(NOT QXYNC_KF_MAJOR MATCHES "^(5|6)$")
    message(FATAL_ERROR "QXYNC_KF_MAJOR must be 5 or 6, got: ${QXYNC_KF_MAJOR}")
endif()

if(QXYNC_KF_MAJOR STREQUAL "6")
    find_package(Qt6 REQUIRED COMPONENTS Core)
    find_package(KF6KIO REQUIRED)
    set(QXYNC_QT_TARGETS Qt6::Core)
    # KF6：KOverlayIconPlugin 在 KIOCore（T14538 从 dolphin 挪进 KIO 后的落点）
    set(QXYNC_KIO_TARGET KF6::KIOCore)
else()
    find_package(Qt5 REQUIRED COMPONENTS Core)
    # KF5：KOverlayIconPlugin 在 KIOWidgets（头文件 KF5/KIOWidgets/KOverlayIconPlugin，
    # staticMetaObject 也在那）。链接口真正所在的库，.so 的 DT_NEEDED 才正确，
    # dpkg-shlibdeps 才能算出真实依赖（链 KIOCore 的话符号悬空、靠宿主进程解析）
    find_package(KF5KIO REQUIRED COMPONENTS KIOCore KIOWidgets)
    set(QXYNC_QT_TARGETS Qt5::Core)
    set(QXYNC_KIO_TARGET KF5::KIOWidgets)
endif()

# Dolphin 从 <Qt 插件目录>/kf<N>/overlayicon 加载 KOverlayIconPlugin
# （Dolphin 源码 src/kitemviews/kfileitemmodelrolesupdater.cpp）。
add_library(qxync_overlay MODULE qxync_overlay.cpp)
target_link_libraries(qxync_overlay PRIVATE ${QXYNC_QT_TARGETS} ${QXYNC_KIO_TARGET})

# 元数据改了要触发 moc 重新生成插件的 Q_PLUGIN_METADATA。
set_property(SOURCE qxync_overlay.cpp PROPERTY OBJECT_DEPENDS "${CMAKE_CURRENT_SOURCE_DIR}/qxync_overlay.json")

install(TARGETS qxync_overlay
        LIBRARY DESTINATION "${CMAKE_INSTALL_LIBDIR}/qt${QXYNC_KF_MAJOR}/plugins/kf${QXYNC_KF_MAJOR}/overlayicon")

# 角标图标：Dolphin 用 QIcon::fromTheme(<getOverlays 返回的名字>) 解析，
# 所以必须进 hicolor 主题（不能直接给路径）。
install(FILES
        icons/qxync-hydrated.svg
        icons/qxync-placeholder.svg
        icons/qxync-partial.svg
        DESTINATION "${CMAKE_INSTALL_DATADIR}/icons/hicolor/scalable/status")
