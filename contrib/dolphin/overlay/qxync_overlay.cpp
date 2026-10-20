/*
    qxync 的 Dolphin 角标插件（KOverlayIconPlugin）

    Dolphin 没有“自定义状态列”的插件接口，但会在文件图标上画 KOverlayIconPlugin
    返回的角标（左下/右上/右下三个角，左上角留给 VCS 插件）。这里把 qxync 的
    FUSE xattr 映射成三个主题图标名：

        user.xdg.tags = ✓（已水合）    → qxync-hydrated   （绿色对钩）
        user.xdg.tags = ☁（仅在线）    → qxync-placeholder（云）
        user.xdg.tags = ◐（部分本地）  → qxync-partial    （半满圆）

    开关与 FUSE 侧同源：`user.xdg.tags` 不存在（ENODATA）= Dolphin 集成关 →
    不画角标，也不读 `user.qxync.state`。目录的 tags 是空串 → 目录不画角标。

    SPDX-License-Identifier: MIT OR Apache-2.0
*/

#include <KOverlayIconPlugin>

#include <QByteArray>
#include <QElapsedTimer>
#include <QFile>
#include <QStringList>
#include <QUrl>

#include <sys/types.h>
#include <sys/xattr.h>

#include <cstring>

namespace {

// /proc/self/mounts 里的挂载点（只认 fstype == fuse.qxync），带 2 秒缓存 ——
// getOverlays 在主线程调用，不能每次都去解析文件。
QStringList qxyncMounts()
{
    static QElapsedTimer timer;
    static QStringList cached;

    if (timer.isValid() && timer.elapsed() < 2000) {
        return cached;
    }
    cached.clear();

    QFile mounts(QStringLiteral("/proc/self/mounts"));
    if (!mounts.open(QIODevice::ReadOnly)) {
        timer.start();
        return cached;
    }
    // 注意：/proc 下的文件 size 是 0，不能用 atEnd() 判终止，只能读到空行为止。
    while (true) {
        const QByteArray line = mounts.readLine();
        if (line.isEmpty()) {
            break;
        }
        const QList<QByteArray> fields = line.split(' ');
        if (fields.size() < 3 || fields.at(2) != "fuse.qxync") {
            continue;
        }
        QByteArray point = fields.at(1);
        point.replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\");
        cached.append(QString::fromUtf8(point));
    }
    timer.start();
    return cached;
}

bool underQxyncMount(const QString &path)
{
    for (const QString &mount : qxyncMounts()) {
        if (path == mount || path.startsWith(mount + QLatin1Char('/'))) {
            return true;
        }
    }
    return false;
}

// 读 xattr；不存在（含 ENODATA / ENOTSUP / 权限不足）一律返回空。
QByteArray readXattr(const QByteArray &path, const char *name)
{
    char buffer[256];
    const ssize_t size = ::getxattr(path.constData(), name, buffer, sizeof(buffer) - 1);
    if (size < 0) {
        return {};
    }
    return QByteArray(buffer, static_cast<int>(size));
}

} // namespace

class QxyncOverlayPlugin : public KOverlayIconPlugin
{
    Q_OBJECT
    Q_PLUGIN_METADATA(IID "org.kde.overlayicon.qxync" FILE "qxync_overlay.json")

public:
    QxyncOverlayPlugin() = default;

    QStringList getOverlays(const QUrl &url) override
    {
        if (!url.isLocalFile()) {
            return {};
        }
        const QString path = url.toLocalFile();
        if (!underQxyncMount(path)) {
            return {};
        }

        const QByteArray encoded = QFile::encodeName(path);
        const QByteArray tags = readXattr(encoded, "user.xdg.tags");
        if (tags.isEmpty()) {
            // 开关关（ENODATA）或目录（空串）：不画角标。
            return {};
        }

        // 语义以 user.qxync.state 为准（placeholder/partial/hydrated）。
        // 这里第二次 getxattr 仍然很便宜：只有 qxync 挂载点内的文件才会走到。
        const QByteArray state = readXattr(encoded, "user.qxync.state");
        if (state == "hydrated") {
            return {QStringLiteral("qxync-hydrated")};
        }
        if (state == "partial") {
            return {QStringLiteral("qxync-partial")};
        }
        if (state == "placeholder") {
            return {QStringLiteral("qxync-placeholder")};
        }
        return {};
    }
};

#include "qxync_overlay.moc"
