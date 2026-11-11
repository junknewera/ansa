#include "omh/local_storage.hpp"
#include <cerrno>
#include <fcntl.h>
#include <linux/fs.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

namespace omh::local {
Fd::~Fd() { reset(); }
Fd::Fd(Fd&& other) noexcept : value_(other.release()) {}
Fd& Fd::operator=(Fd&& other) noexcept { if (this != &other) reset(other.release()); return *this; }
int Fd::release() { const int value = value_; value_ = -1; return value; }
void Fd::reset(int value) { if (value_ >= 0) ::close(value_); value_ = value; }
void sync(int fd) {
    int result;
    do { result = ::fsync(fd); } while (result < 0 && errno == EINTR);
    require(result == 0, "local_sync", Stage::execution);
}
void write_all(int fd, std::string_view value) {
    while (!value.empty()) {
        const auto count = ::write(fd, value.data(), value.size());
        if (count < 0 && errno == EINTR) continue;
        require(count > 0, "local_write", Stage::execution);
        value.remove_prefix(static_cast<std::size_t>(count));
    }
}
void private_directory(const std::filesystem::path& path) {
    require(path.is_absolute() && !path.empty(), "local_directory_path", Stage::execution);
    std::vector<std::filesystem::path> missing;
    auto parent = path;
    while (!std::filesystem::exists(parent)) {
        missing.push_back(parent); parent = parent.parent_path();
        require(!parent.empty(), "local_directory_path", Stage::execution);
    }
    for (auto it = missing.rbegin(); it != missing.rend(); ++it) {
        require(::mkdir(it->c_str(), 0700) == 0 || errno == EEXIST, "local_directory_create", Stage::execution);
        Fd directory(::open(it->parent_path().c_str(), O_DIRECTORY | O_RDONLY | O_CLOEXEC));
        require(directory.get() >= 0, "local_directory_open", Stage::execution); sync(directory.get());
    }
}
Fd open_nofollow(const std::filesystem::path& path, int flags) {
    if (!path.is_absolute()) { errno = EINVAL; return Fd{}; }
    const auto normalized = path.lexically_normal();
    Fd directory(::open("/", O_RDONLY | O_DIRECTORY | O_CLOEXEC));
    if (directory.get() < 0) return directory;
    for (const auto& part : normalized.parent_path().relative_path()) {
        Fd next(::openat(directory.get(), part.c_str(), O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW));
        if (next.get() < 0) return next;
        directory = std::move(next);
    }
    return Fd(::openat(directory.get(), normalized.filename().c_str(), flags | O_NOFOLLOW | O_CLOEXEC));
}
Fd open_private_directory(const std::filesystem::path& path) {
    Fd directory = open_nofollow(path, O_RDONLY | O_DIRECTORY);
    struct stat status{};
    require(directory.get() >= 0 && ::fstat(directory.get(), &status) == 0 && S_ISDIR(status.st_mode) &&
            status.st_uid == ::geteuid() && (status.st_mode & 077) == 0, "local_directory_permissions", Stage::execution);
    return directory;
}
std::optional<Json> read_json(int directory, const char* name, std::size_t limit) {
    Fd file(::openat(directory, name, O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK));
    if (file.get() < 0 && errno == ENOENT) return {};
    struct stat status{};
    require(file.get() >= 0 && ::fstat(file.get(), &status) == 0 && S_ISREG(status.st_mode) &&
            status.st_uid == ::geteuid() && status.st_nlink == 1 && (status.st_mode & 077) == 0 &&
            status.st_size > 0 && static_cast<std::uint64_t>(status.st_size) <= limit,
            "local_file_permissions", Stage::execution);
    std::string bytes;
    char chunk[4096];
    for (;;) {
        const auto size = ::read(file.get(), chunk, sizeof(chunk));
        if (size < 0 && errno == EINTR) continue;
        require(size >= 0, "local_file_read", Stage::execution);
        if (!size) break;
        require(static_cast<std::size_t>(size) <= limit - bytes.size(), "local_file_limit", Stage::execution);
        bytes.append(chunk, static_cast<std::size_t>(size));
    }
    return Json::parse(bytes, limit);
}
void create_json(int directory, const char* name, const Json& value) {
    // Publish only complete, synced records. Existing evidence is never replaced.
    const auto temporary = std::string(name) + ".pending";
    Fd file(::openat(directory, temporary.c_str(), O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0600));
    require(file.get() >= 0, "local_file_create", Stage::execution);
    write_all(file.get(), value.dump()); sync(file.get());
    require(::syscall(SYS_renameat2, directory, temporary.c_str(), directory, name, RENAME_NOREPLACE) == 0,
            "local_file_publish", Stage::execution);
    sync(directory);
}
std::string utf8_text(std::string_view input) {
    std::string out;
    out.reserve(input.size());
    for (std::size_t i = 0; i < input.size();) {
        const auto lead = static_cast<unsigned char>(input[i]);
        std::size_t length = lead < 0x80 ? 1 : lead >= 0xc2 && lead <= 0xdf ? 2 :
            lead >= 0xe0 && lead <= 0xef ? 3 : lead >= 0xf0 && lead <= 0xf4 ? 4 : 0;
        bool valid = length && length <= input.size() - i;
        for (std::size_t n = 1; valid && n < length; ++n)
            valid = (static_cast<unsigned char>(input[i + n]) & 0xc0) == 0x80;
        if (valid && length >= 3) {
            const auto second = static_cast<unsigned char>(input[i + 1]);
            valid = !(lead == 0xe0 && second < 0xa0) && !(lead == 0xed && second >= 0xa0) &&
                !(lead == 0xf0 && second < 0x90) && !(lead == 0xf4 && second >= 0x90);
        }
        if (valid) { out.append(input.substr(i, length)); i += length; }
        else { out += "\xef\xbf\xbd"; ++i; }
    }
    return out;
}
bool protected_path(const std::filesystem::path& path) {
    for (const auto& part : path) {
        const auto name = part.string();
        if (name == ".env" || name.starts_with(".env.") || name == ".codex" ||
            name == ".claude" || name == ".gemini") return true;
    }
    return false;
}
}
