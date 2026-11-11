#pragma once
#include "omh/json.hpp"
#include <filesystem>
#include <optional>

namespace omh::local {
class Fd {
    int value_ = -1;
public:
    explicit Fd(int value = -1) : value_(value) {}
    ~Fd();
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    Fd(Fd&&) noexcept;
    Fd& operator=(Fd&&) noexcept;
    int get() const { return value_; }
    int release();
    void reset(int value = -1);
};
void sync(int fd);
void write_all(int fd, std::string_view);
void private_directory(const std::filesystem::path&);
Fd open_nofollow(const std::filesystem::path&, int flags);
Fd open_private_directory(const std::filesystem::path&);
std::optional<Json> read_json(int directory, const char* name, std::size_t limit = 128 * 1024);
void create_json(int directory, const char* name, const Json&);
std::string utf8_text(std::string_view);
bool protected_path(const std::filesystem::path&);
}
