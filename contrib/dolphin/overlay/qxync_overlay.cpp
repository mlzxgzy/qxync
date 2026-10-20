/*
    qxync 的 Dolphin 角标插件（KOverlayIconPlugin）

    Dolphin 没有“自定义状态列”的插件接口，但会在文件图标上画 KOverlayIconPlugin
    返回的角标（左下/右上/右下三个角，左上角留给 VCS 插件）。这里把 qxync 的
    FUSE xattr 映射成三个主题图标名：

        user.qxync.overlay = hydrated     → qxync-hydrated   （绿色对钩）
        user.qxync.overlay = placeholder  → qxync-placeholder（云）
        user.qxync.overlay = partial      → qxync-partial    （半满圆）

    开关与 FUSE 侧同源：`user.qxync.overlay` 不存在（ENODATA）= 角标开关关着
    （或该挂载点会被 Baloo 索引而不暴露）→ 不画角标。目录的值是空串 → 不画。

    为什么读专用属性而不是认 Tags 字符：展示与语义解耦 —— 以后改 Tags 列的
    字面/图标字符不会让角标跟着坏；而且 Tags 列开关与角标开关是**独立**的。

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

        // 单次 getxattr：`user.qxync.overlay` 是 FUSE 按「角标开关开 && 挂载点
        // 不会被 Baloo 索引」现算的专用属性 —— 开关关着时根本不存在（ENODATA），
        // 目录是空串，文件的值就是三态本身（placeholder / partial / hydrated）。
        const QByteArray state = readXattr(QFile::encodeName(path), "user.qxync.overlay");
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
