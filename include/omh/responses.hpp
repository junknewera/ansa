#pragma once
#include "omh/sse.hpp"
#include <map>
#include <optional>
#include <set>
#include <stop_token>

namespace omh {
inline constexpr std::size_t max_request_items = 4096;
struct Binding {
    std::string provider = "openai-siwc";
    std::string account = "offline-fixture";
    std::string requested_model = "gpt-6.1-sol";
    // Exact, explicit returned-model policy. No prefix/substring model matching.
    std::string returned_model = "gpt-6.1-sol";
    bool operator==(const Binding&) const = default;
};
struct ToolDescriptor {
    std::string name, description, implementation_revision;
    Json schema;
};
struct ToolAdvertisement {
    std::string revision;
    std::vector<ToolDescriptor> descriptors; // Deliberately no validators/executors.
};
struct ContextEpoch {
    std::string id, instructions;
    Json exact_state; // Exact constraints/provenance/config, not just an instruction hash.
};
class Request {
    Binding binding_;
    ContextEpoch epoch_;
    ToolAdvertisement tools_;
    std::vector<Json> input_;
public:
    Request(Binding, ContextEpoch, ToolAdvertisement, std::vector<Json> input);
    const Binding& binding() const { return binding_; }
    const ContextEpoch& epoch() const { return epoch_; }
    const ToolAdvertisement& tools() const { return tools_; }
    const std::vector<Json>& input() const { return input_; }
    Json json() const;
};
struct FunctionCall { std::string item_id, call_id, name, arguments; std::size_t output_index; };
class ResponsesReducer;
class CompletedResponse {
    Request request_;
    std::string id_, returned_model_;
    std::vector<Json> items_;
    std::vector<FunctionCall> calls_;
    CompletedResponse(Request, std::string id, std::string model, std::vector<Json>, std::vector<FunctionCall>);
    friend class ResponsesReducer;
public:
    const Request& request() const { return request_; }
    const std::string& id() const { return id_; }
    const std::string& returned_model() const { return returned_model_; }
    const std::vector<Json>& items() const { return items_; }
    const std::vector<FunctionCall>& calls() const { return calls_; }
};
class ResponseStream;
class ResponsesReducer {
    struct Item {
        struct Part {
            Json added;
            std::optional<Json> done;
            std::string delta;
            std::optional<std::string> final_text;
            bool saw_delta = false;
        };
        Json added;
        std::optional<Json> done;
        std::string delta;
        std::optional<std::string> final_args;
        std::map<std::size_t, Part> parts;
        bool saw_delta = false;
    };
    Request request_;
    Limits limits_;
    std::string id_, model_;
    std::map<std::size_t, Item> items_;
    std::map<std::string, std::size_t> ids_;
    std::set<std::string> call_ids_;
    std::optional<Json> terminal_;
    std::size_t sequence_ = 0, bytes_ = 0;
    bool created_ = false, progress_ = false, sealed_ = false;
    Item& target(const Json&);
    void identity(const Json&) const;
    void validate_item(const Json&, bool final) const;
    CompletedResponse seal();
    friend class ResponseStream;
public:
    explicit ResponsesReducer(Request, Limits = {});
    void event(SseEvent);
};
class ResponseStream {
    ResponsesReducer reducer_;
    SseParser framer_;
    std::stop_token stop_;
    bool closed_ = false;
public:
    explicit ResponseStream(Request, Limits = {}, std::stop_token = {});
    ResponseStream(const ResponseStream&) = delete;
    ResponseStream& operator=(const ResponseStream&) = delete;
    void feed(std::string_view);
    Result<CompletedResponse> finish();
};
enum class Execution { succeeded, failed, cancelled, denied };
enum class Effects { not_started, known_result, unknown };
struct ToolResult {
    std::string operation_id, call_id, output;
    Execution execution;
    Effects effects;
    bool dispatched = false;
};
Request continuation(const CompletedResponse&, const std::vector<ToolResult>&);
} // namespace omh
