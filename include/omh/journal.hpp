#pragma once
#include "omh/json.hpp"
#include <filesystem>
#include <functional>

namespace omh {
enum class JournalPoint { before_write, after_write, before_sync, after_sync };
struct JournalOptions {
    // Fault/barrier injection for storage tests. Never receives record contents.
    std::function<void(JournalPoint, std::string_view kind)> observe;
};

// One process holds an exclusive session lock for this object's lifetime.
// Records and recovered tails remain append-only. Torn tails start a new part.
class Journal {
    class Impl;
    std::unique_ptr<Impl> impl_;
public:
    Journal(const std::filesystem::path&, const std::function<void(const Json&)>& replay, JournalOptions = {});
    ~Journal();
    Journal(const Journal&) = delete;
    Journal& operator=(const Journal&) = delete;
    bool empty() const;
    const std::string& diagnostic() const;
    void append(const Json&);
};
} // namespace omh
