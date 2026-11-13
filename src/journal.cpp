#include "omh/journal.hpp"
#include <openssl/evp.h>
#include <algorithm>
#include <array>
#include <cerrno>
#include <charconv>
#include <fcntl.h>
#include <limits>
#include <sys/file.h>
#include <sys/stat.h>
#include <unistd.h>

namespace omh {
namespace {
constexpr std::size_t max_record = 32 * 1024 * 1024;
constexpr std::size_t frame_header = 83;
const std::string zero_hash(64, '0');

class Fd {
    int fd_ = -1;
public:
    explicit Fd(int fd = -1) : fd_(fd) {}
    ~Fd() { if (fd_ >= 0) ::close(fd_); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int get() const { return fd_; }
    void reset(int fd) { if (fd_ >= 0) ::close(fd_); fd_ = fd; }
};

void storage_require(bool ok, const char* diagnostic) {
    require(ok, diagnostic, Stage::frontend, ErrorCode::contract);
}

std::string digits(std::uint64_t value, std::size_t width, int base = 10) {
    std::array<char, 32> buffer{};
    const auto result = std::to_chars(buffer.data(), buffer.data() + buffer.size(), value, base);
    storage_require(result.ec == std::errc{}, "journal_number");
    std::string text(buffer.data(), result.ptr);
    storage_require(text.size() <= width, "journal_number");
    return std::string(width - text.size(), '0') + text;
}

std::string part_name(std::uint64_t index) { return "part-" + digits(index, 20) + ".log"; }
std::string part_header(std::uint64_t index, const std::string& previous) {
    return "OMHJ1 " + digits(index, 20) + " " + previous + "\n";
}

std::string read_bytes(int fd, std::size_t count) {
    std::string bytes(count, '\0');
    std::size_t used = 0;
    while (used < count) {
        const auto n = ::read(fd, bytes.data() + used, count - used);
        if (n < 0 && errno == EINTR) continue;
        storage_require(n >= 0, "journal_read");
        if (n == 0) break;
        used += static_cast<std::size_t>(n);
    }
    bytes.resize(used);
    return bytes;
}

void write_bytes(int fd, std::string_view bytes) {
    while (!bytes.empty()) {
        const auto n = ::write(fd, bytes.data(), bytes.size());
        if (n < 0 && errno == EINTR) continue;
        storage_require(n > 0, "journal_write");
        bytes.remove_prefix(static_cast<std::size_t>(n));
    }
}

void sync_fd(int fd) {
    int result;
    do { result = ::fsync(fd); } while (result < 0 && errno == EINTR);
    storage_require(result == 0, "journal_sync");
}

std::string file_digest(int fd) {
    auto context = std::unique_ptr<EVP_MD_CTX, decltype(&EVP_MD_CTX_free)>(EVP_MD_CTX_new(), EVP_MD_CTX_free);
    storage_require(context && EVP_DigestInit_ex(context.get(), EVP_sha256(), nullptr) == 1, "journal_hash");
    std::array<char, 64 * 1024> buffer{};
    off_t offset = 0;
    while (true) {
        const auto n = ::pread(fd, buffer.data(), buffer.size(), offset);
        if (n < 0 && errno == EINTR) continue;
        storage_require(n >= 0, "journal_read");
        if (!n) break;
        storage_require(EVP_DigestUpdate(context.get(), buffer.data(), static_cast<std::size_t>(n)) == 1,
                        "journal_hash");
        offset += n;
    }
    std::array<unsigned char, EVP_MAX_MD_SIZE> digest{};
    unsigned size = 0;
    storage_require(EVP_DigestFinal_ex(context.get(), digest.data(), &size) == 1 && size == 32, "journal_hash");
    std::string hex;
    for (unsigned i = 0; i < size; ++i) hex += digits(digest[i], 2, 16);
    return hex;
}

std::uint32_t parse_length(std::string_view bytes) {
    std::uint32_t value = 0;
    const auto result = std::from_chars(bytes.data(), bytes.data() + bytes.size(), value, 16);
    storage_require(result.ec == std::errc{} && result.ptr == bytes.data() + bytes.size(), "journal_frame");
    return value;
}

Json number(std::uint64_t value) { return Json::parse(std::to_string(value)); }
} // namespace

class Journal::Impl {
    Fd directory_, lock_, file_;
    JournalOptions options_;
    std::uint64_t part_ = 0, sequence_ = 0;
    std::string previous_part_ = zero_hash, previous_record_ = zero_hash, diagnostic_;
    off_t expected_size_ = 0;
    bool new_part_ = true, failed_ = false;

    void scan(int fd, std::uint64_t index, const std::function<void(const Json&)>& replay) {
        const auto expected = part_header(index, previous_part_);
        const auto header = read_bytes(fd, expected.size());
        storage_require(expected.starts_with(header), "journal_part_header");
        if (header.size() != expected.size()) { new_part_ = true; return; }
        while (true) {
            const auto frame = read_bytes(fd, frame_header);
            if (frame.empty()) { new_part_ = false; return; }
            if (frame.size() < frame_header) { new_part_ = true; return; }
            storage_require(frame[8] == ' ' && frame[17] == ' ' && frame[82] == '\n', "journal_frame");
            const auto length = parse_length(std::string_view(frame).substr(0, 8));
            const auto complement = parse_length(std::string_view(frame).substr(9, 8));
            storage_require((length ^ complement) == std::numeric_limits<std::uint32_t>::max() &&
                            length > 0 && length <= max_record, "journal_frame_length");
            const auto body = read_bytes(fd, length);
            if (body.size() < length) { new_part_ = true; return; }
            const auto hash = sha256(body);
            storage_require(hash == frame.substr(18, 64), "journal_checksum");
            const auto envelope = Json::parse(body, max_record);
            storage_require(envelope.index("schema_version") == 1 && sequence_ < std::numeric_limits<std::uint64_t>::max() &&
                            envelope.index("sequence") == sequence_ + 1 &&
                            envelope.text("previous") == previous_record_, "journal_sequence");
            replay(envelope.at("event"));
            previous_record_ = hash;
            ++sequence_;
        }
    }

    void open_part() {
        if (!new_part_) return;
        storage_require(part_ < 4096, "journal_parts_limit");
        const auto name = part_name(part_);
        file_.reset(::openat(directory_.get(), name.c_str(), O_WRONLY | O_CREAT | O_EXCL | O_APPEND |
                             O_CLOEXEC | O_NOFOLLOW, 0600));
        storage_require(file_.get() >= 0, "journal_part_create");
        const auto header = part_header(part_, previous_part_);
        write_bytes(file_.get(), header);
        sync_fd(file_.get());
        sync_fd(directory_.get());
        expected_size_ = static_cast<off_t>(header.size());
        new_part_ = false;
    }

public:
    Impl(const std::filesystem::path& path, const std::function<void(const Json&)>& replay, JournalOptions options)
        : options_(std::move(options)) {
        storage_require(!path.empty() && path.native().find('\0') == std::string::npos, "journal_path");
        storage_require(::mkdir(path.c_str(), 0700) == 0 || errno == EEXIST, "journal_directory_create");
        directory_.reset(::open(path.c_str(), O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW));
        storage_require(directory_.get() >= 0, "journal_directory_open");
        const auto parent_path = path.parent_path().empty() ? std::filesystem::path(".") : path.parent_path();
        Fd parent(::open(parent_path.c_str(), O_RDONLY | O_DIRECTORY | O_CLOEXEC));
        storage_require(parent.get() >= 0, "journal_parent_open");
        sync_fd(parent.get());
        lock_.reset(::openat(directory_.get(), "lock", O_RDWR | O_CREAT | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK, 0600));
        storage_require(lock_.get() >= 0, "journal_lock_open");
        struct stat lock_status{};
        storage_require(::fstat(lock_.get(), &lock_status) == 0 && S_ISREG(lock_status.st_mode) && lock_status.st_nlink == 1, "journal_lock_type");
        storage_require(::flock(lock_.get(), LOCK_EX | LOCK_NB) == 0, "session_busy");

        std::vector<std::string> parts;
        for (const auto& entry : std::filesystem::directory_iterator(path)) {
            const auto name = entry.path().filename().string();
            if (name.starts_with("part-") && name.ends_with(".log")) {
                storage_require(parts.size() < 4096, "journal_parts_limit");
                parts.push_back(name);
            }
        }
        storage_require(parts.size() <= 4096, "journal_parts_limit");
        std::sort(parts.begin(), parts.end());
        try {
            for (std::size_t index = 0; index < parts.size(); ++index) {
                storage_require(parts[index] == part_name(index), "journal_part_sequence");
                file_.reset(::openat(directory_.get(), parts[index].c_str(), O_RDWR | O_APPEND | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK));
                storage_require(file_.get() >= 0, "journal_part_open");
                struct stat status{};
                storage_require(::fstat(file_.get(), &status) == 0 && S_ISREG(status.st_mode) && status.st_nlink == 1, "journal_part_type");
                scan(file_.get(), index, replay);
                expected_size_ = status.st_size;
                previous_part_ = file_digest(file_.get());
                part_ = index;
            }
            if (!parts.empty() && new_part_) {
                ++part_;
                file_.reset(-1);
            }
        } catch (const Error& error) {
            diagnostic_ = error.diagnostic;
            failed_ = true;
        }
    }

    bool empty() const { return sequence_ == 0; }
    const std::string& diagnostic() const { return diagnostic_; }

    void append(const Json& event) {
        storage_require(!failed_, "journal_read_only");
        try {
            open_part();
            struct stat status{}, named{};
            const auto name = part_name(part_);
            storage_require(::fstat(file_.get(), &status) == 0 &&
                            ::fstatat(directory_.get(), name.c_str(), &named, AT_SYMLINK_NOFOLLOW) == 0 &&
                            S_ISREG(named.st_mode) && status.st_nlink == 1 && status.st_dev == named.st_dev && status.st_ino == named.st_ino &&
                            status.st_size == expected_size_, "journal_write_conflict");
            storage_require(sequence_ < std::numeric_limits<std::uint64_t>::max(), "journal_sequence_limit");
            const auto body = Json::object({{"schema_version", number(1)}, {"sequence", number(sequence_ + 1)},
                {"previous", Json::string(previous_record_)}, {"event", event}}).dump();
            storage_require(body.size() <= max_record, "journal_record_limit");
            const auto length = static_cast<std::uint32_t>(body.size());
            const auto hash = sha256(body);
            const auto header = digits(length, 8, 16) + " " +
                digits(length ^ std::numeric_limits<std::uint32_t>::max(), 8, 16) + " " + hash + "\n";
            const auto kind = event.text("kind");
            auto observe = [&](JournalPoint point) { if (options_.observe) options_.observe(point, kind); };
            observe(JournalPoint::before_write);
            write_bytes(file_.get(), header);
            write_bytes(file_.get(), body);
            observe(JournalPoint::after_write);
            observe(JournalPoint::before_sync);
            sync_fd(file_.get());
            observe(JournalPoint::after_sync);
            expected_size_ += static_cast<off_t>(header.size() + body.size());
            previous_record_ = hash;
            ++sequence_;
        } catch (...) {
            failed_ = true;
            throw;
        }
    }
};

Journal::Journal(const std::filesystem::path& path, const std::function<void(const Json&)>& replay, JournalOptions options)
    : impl_(std::make_unique<Impl>(path, replay, std::move(options))) {}
Journal::~Journal() = default;
bool Journal::empty() const { return impl_->empty(); }
const std::string& Journal::diagnostic() const { return impl_->diagnostic(); }
void Journal::append(const Json& event) { impl_->append(event); }
} // namespace omh
