#include "omh/responses.hpp"
#include <algorithm>

namespace omh {
namespace {
void nonempty(const std::string& s) { require(!s.empty() && s.size() <= 256, "invalid_identity"); }
void same_field(const Json& a, const Json& b, const char* field) {
    require(a.has(field) == b.has(field), "identity_field_presence");
    if (a.has(field)) require(a.at(field) == b.at(field), "identity_field_conflict");
}
void compatible_added(const Json& a, const Json& b) {
    for (const auto& key : a.keys()) {
        if (key == "status" || key == "arguments" || key == "content" || key == "summary" || key == "encrypted_content") continue;
        require(b.has(key) && a.at(key) == b.at(key), "added_metadata_conflict");
    }
}
void append_bounded(std::string& buffer, const std::string& value, std::size_t limit) {
    require(value.size() <= limit && buffer.size() <= limit - value.size(), "delta_limit", Stage::admission, ErrorCode::limit);
    buffer += value;
}
bool advertised(const Request& request, const std::string& name) {
    return std::any_of(request.tools().descriptors.begin(), request.tools().descriptors.end(),
                       [&](const auto& d) { return d.name == name; });
}
} // namespace
Request::Request(Binding binding, ContextEpoch epoch, ToolAdvertisement tools, std::vector<Json> input)
    : binding_(std::move(binding)), epoch_(std::move(epoch)), tools_(std::move(tools)), input_(std::move(input)) {
    require(binding_.provider == "openai-siwc" && binding_.requested_model == "gpt-6.1-sol",
            "unsupported_binding", Stage::request);
    nonempty(binding_.account); nonempty(binding_.returned_model); nonempty(epoch_.id);
    require(epoch_.exact_state.is_object() && !tools_.revision.empty(), "request_context", Stage::request);
    require(input_.size() <= 4096 && tools_.descriptors.size() <= 64, "request_items_limit", Stage::request, ErrorCode::limit);
    for (const auto& item : input_) {
        require(item.is_object(), "input_item_object", Stage::request);
        if (item.has("role")) require(item.text("role") != "system", "siwc_system_role", Stage::request);
    }
}
Json Request::json() const {
    std::vector<Json> tools;
    for (const auto& d : tools_.descriptors) {
        tools.push_back(Json::object({{"type", Json::string("function")}, {"name", Json::string(d.name)},
            {"description", Json::string(d.description)}, {"strict", Json::parse("true")}, {"parameters", d.schema}}));
    }
    const auto namespaced = Json::object({{"type", Json::string("namespace")}, {"name", Json::string("omh")},
        {"description", Json::string("Compiled local tools")}, {"tools", Json::array(tools)}});
    return Json::object({{"model", Json::string(binding_.requested_model)}, {"store", Json::parse("false")},
        {"stream", Json::parse("true")}, {"instructions", Json::string(epoch_.instructions)},
        {"input", Json::array(input_)}, {"tools", Json::array({namespaced})},
        {"include", Json::array({Json::string("reasoning.encrypted_content")})}});
}
CompletedResponse::CompletedResponse(Request request, std::string id, std::string model,
                                     std::vector<Json> items, std::vector<FunctionCall> calls)
    : request_(std::move(request)), id_(std::move(id)), returned_model_(std::move(model)),
      items_(std::move(items)), calls_(std::move(calls)) {}
ResponsesReducer::ResponsesReducer(Request request, Limits limits) : request_(std::move(request)), limits_(limits) {}
void ResponsesReducer::identity(const Json& response) const {
    require(response.text("id") == id_, "response_identity");
    require(response.text("model") == model_, "response_model_conflict");
}
void ResponsesReducer::validate_item(const Json& item, bool final) const {
    require(item.is_object(), "item_object"); nonempty(item.text("id"));
    const auto type = item.text("type");
    require(type == "function_call" || type == "reasoning" || type == "message", "unsupported_output_item");
    if (item.has("status")) {
        const auto status = item.text("status");
        require(final ? status == "completed" : (status == "in_progress" || status == "completed"), "item_status");
    }
    if (type == "function_call") {
        nonempty(item.text("call_id")); nonempty(item.text("name"));
        require(item.text("namespace") == "omh" && advertised(request_, item.text("name")), "unadvertised_function");
        const auto args = item.text("arguments");
        require(args.size() <= limits_.arguments, "arguments_limit", Stage::admission, ErrorCode::limit);
        if (final) {
            require(item.has("status") && item.text("status") == "completed", "function_status");
            require(Json::parse(args, limits_.arguments, limits_.json_depth, limits_.json_nodes).is_object(), "arguments_object");
        }
    } else if (type == "reasoning") {
        require(item.at("summary").is_array(), "reasoning_summary");
        if (final) require(!item.text("encrypted_content").empty(), "reasoning_missing_opaque");
        else if (item.has("encrypted_content")) (void)item.text("encrypted_content");
        for (const auto& part : item.at("summary").elements()) {
            require(part.text("type") == "summary_text", "reasoning_summary_type"); (void)part.text("text");
        }
    } else {
        require(item.text("role") == "assistant" && item.at("content").is_array(), "message_shape");
        if (final) require(item.has("status") && item.text("status") == "completed", "message_status");
        for (const auto& part : item.at("content").elements()) {
            const auto kind = part.text("type");
            require(kind == "output_text" || kind == "refusal", "unsupported_content");
            (void)part.text(kind == "refusal" ? "refusal" : "text");
        }
    }
}
ResponsesReducer::Item& ResponsesReducer::target(const Json& event) {
    const auto id = event.text("item_id");
    const auto it = ids_.find(id);
    require(it != ids_.end(), "orphan_item_id");
    require(event.index("output_index") == it->second, "item_index_conflict");
    auto& item = items_.at(it->second);
    require(!item.done, "event_after_item_done");
    for (const char* field : {"call_id", "name", "namespace"})
        if (event.has(field)) require(item.added.has(field) && event.at(field) == item.added.at(field), "event_call_identity_conflict");
    return item;
}
void ResponsesReducer::event(SseEvent wire) {
    require(!sealed_ && !terminal_, "event_after_terminal");
    require(wire.data.size() <= limits_.event && wire.data.size() <= limits_.body - bytes_,
            "response_byte_limit", Stage::admission, ErrorCode::limit);
    bytes_ += wire.data.size();
    const auto e = Json::parse(wire.data, limits_.event, limits_.json_depth, limits_.json_nodes);
    require(e.is_object(), "event_object");
    const auto type = e.text("type");
    require(wire.name == "message" || wire.name == type, "sse_event_name_conflict");
    require(sequence_ < limits_.events && e.index("sequence_number") == sequence_++, "event_sequence");
    if (e.has("response_id")) require(created_ && e.text("response_id") == id_, "event_response_identity");
    if (type == "response.created") {
        require(!created_ && items_.empty(), "duplicate_created");
        const auto response = e.at("response");
        id_ = response.text("id"); model_ = response.text("model"); nonempty(id_);
        require(model_ == request_.binding().returned_model, "wrong_returned_model");
        require(response.text("status") == "in_progress" && response.at("output").elements().empty(), "created_status");
        created_ = true; return;
    }
    require(created_, "event_before_created");
    if (type == "response.failed" || type == "response.incomplete" || type == "error")
        fail(type == "response.failed" ? ErrorCode::response_failed :
             type == "response.incomplete" ? ErrorCode::response_incomplete : ErrorCode::protocol, Stage::admission,
             type == "response.failed" ? "response_failed" : type == "response.incomplete" ? "response_incomplete" : "response_error");
    if (type == "response.in_progress") {
        require(!progress_, "duplicate_in_progress"); progress_ = true;
        const auto response = e.at("response"); identity(response);
        require(response.text("status") == "in_progress" && response.at("output").elements().empty(), "progress_status");
    } else if (type == "response.output_item.added") {
        const auto index = e.index("output_index");
        require(index == items_.size() && index < limits_.items, "item_order_or_limit");
        auto item = e.at("item"); validate_item(item, false);
        require(ids_.emplace(item.text("id"), index).second, "duplicate_item_id");
        if (item.text("type") == "function_call")
            require(call_ids_.size() < limits_.calls && call_ids_.insert(item.text("call_id")).second, "duplicate_call_or_limit");
        items_.emplace(index, Item{std::move(item), {}, {}, {}, {}, false});
    } else if (type == "response.function_call_arguments.delta" || type == "response.function_call_arguments.done") {
        auto& item = target(e);
        require(item.added.text("type") == "function_call" && !item.final_args, "arguments_event_conflict");
        if (type.ends_with(".delta")) {
            item.saw_delta = true; append_bounded(item.delta, e.text("delta"), limits_.arguments);
        } else {
            auto args = e.text("arguments");
            require(args.size() <= limits_.arguments, "arguments_limit", Stage::admission, ErrorCode::limit);
            require(!item.saw_delta || item.delta == args, "arguments_done_conflict");
            item.final_args = std::move(args);
        }
    } else if (type == "response.output_item.done") {
        const auto index = e.index("output_index");
        const auto it = items_.find(index); require(it != items_.end(), "orphan_done");
        auto& state = it->second; require(!state.done, "duplicate_item_done");
        auto item = e.at("item"); validate_item(item, true);
        same_field(state.added, item, "id"); same_field(state.added, item, "type");
        compatible_added(state.added, item);
        if (item.text("type") == "function_call") {
            for (const char* field : {"call_id", "name", "namespace"}) same_field(state.added, item, field);
            const auto args = item.text("arguments");
            const auto initial = state.added.text("arguments");
            require(initial.empty() || initial == args, "added_arguments_conflict");
            require(!state.saw_delta || state.delta == args, "arguments_delta_conflict");
            require(!state.final_args || *state.final_args == args, "arguments_final_conflict");
        } else {
            const bool reasoning = item.text("type") == "reasoning";
            const auto parts = item.at(reasoning ? "summary" : "content").elements();
            const auto initial = state.added.at(reasoning ? "summary" : "content").elements();
            require(initial.empty() || initial == parts, "added_content_conflict");
            if (reasoning && state.added.has("encrypted_content")) {
                const auto opaque = state.added.text("encrypted_content");
                require(opaque.empty() || opaque == item.text("encrypted_content"), "opaque_conflict");
            }
            if (!state.parts.empty()) {
                require(state.parts.size() == parts.size(), "content_part_count");
                for (std::size_t i = 0; i < parts.size(); ++i) {
                    const auto p = state.parts.find(i);
                    require(p != state.parts.end() && p->second.done && *p->second.done == parts[i], "content_part_final_conflict");
                }
            }
        }
        state.done = std::move(item);
    } else if (type == "response.completed") {
        const auto response = e.at("response"); identity(response);
        require(response.text("status") == "completed", "terminal_status");
        if (response.has("error")) require(response.at("error").is_null(), "completed_with_error");
        if (response.has("incomplete_details")) require(response.at("incomplete_details").is_null(), "completed_with_incomplete");
        const auto output = response.at("output").elements();
        require(output.size() == items_.size(), "terminal_item_count");
        for (std::size_t i = 0; i < output.size(); ++i) {
            const auto& item = items_.at(i);
            require(item.done && *item.done == output[i], "terminal_item_conflict");
        }
        terminal_ = response;
    } else {
        // Closed, documented subset of message/summary events. Unknown events fail closed.
        const bool summary = type.starts_with("response.reasoning_summary_");
        const bool part_event = type == "response.content_part.added" || type == "response.content_part.done" ||
            type == "response.reasoning_summary_part.added" || type == "response.reasoning_summary_part.done";
        const bool text_event = type == "response.output_text.delta" || type == "response.output_text.done" ||
            type == "response.refusal.delta" || type == "response.refusal.done" ||
            type == "response.reasoning_summary_text.delta" || type == "response.reasoning_summary_text.done";
        require(part_event || text_event, "unsupported_response_event");
        auto& state = target(e);
        require(state.added.text("type") == (summary ? "reasoning" : "message"), "content_item_type");
        const auto index = e.index(summary ? "summary_index" : "content_index");
        require(index < limits_.items, "content_index_limit");
        if (part_event && type.ends_with(".added")) {
            require(index == state.parts.size(), "content_part_order");
            auto part = e.at("part");
            const auto expected = summary ? "summary_text" : part.text("type");
            require(expected == "summary_text" || expected == "output_text" || expected == "refusal", "content_part_type");
            require(part.text("type") == expected && part.text(expected == "refusal" ? "refusal" : "text").empty(), "content_part_added");
            require(state.parts.emplace(index, Item::Part{std::move(part), {}, {}, {}, false}).second, "duplicate_part");
        } else {
            auto p = state.parts.find(index); require(p != state.parts.end() && !p->second.done, "orphan_or_closed_part");
            auto& part = p->second;
            const auto kind = part.added.text("type");
            const auto key = kind == "refusal" ? "refusal" : "text";
            if (part_event) {
                auto final = e.at("part");
                require(final.text("type") == kind && part.final_text && final.text(key) == *part.final_text, "part_done_conflict");
                part.done = std::move(final);
            } else {
                require(!part.final_text, "text_after_done");
                require((type.starts_with("response.refusal.") && kind == "refusal") ||
                    (summary && kind == "summary_text") || (type.starts_with("response.output_text.") && kind == "output_text"), "text_type_conflict");
                if (type.ends_with(".delta")) { part.saw_delta = true; append_bounded(part.delta, e.text("delta"), limits_.body); }
                else {
                    const auto text = e.text(key);
                    require(!part.saw_delta || part.delta == text, "text_done_conflict");
                    part.final_text = text;
                }
            }
        }
    }
}
CompletedResponse ResponsesReducer::seal() {
    require(!sealed_ && terminal_.has_value(), "eof_before_completed", Stage::admission, ErrorCode::premature_eof); sealed_ = true;
    auto output = terminal_->at("output").elements();
    std::vector<FunctionCall> calls;
    for (std::size_t i = 0; i < output.size(); ++i) {
        const auto& item = output[i];
        if (item.text("type") == "function_call") calls.push_back({item.text("id"), item.text("call_id"),
            item.text("name"), item.text("arguments"), i});
    }
    return CompletedResponse(request_, id_, model_, std::move(output), std::move(calls));
}
ResponseStream::ResponseStream(Request request, Limits limits, std::stop_token stop)
    : reducer_(std::move(request), limits), framer_([this](SseEvent e) { reducer_.event(std::move(e)); }, limits), stop_(stop) {}
void ResponseStream::feed(std::string_view bytes) {
    require(!closed_, "stream_closed");
    require(!stop_.stop_requested(), "stream_cancelled", Stage::admission, ErrorCode::cancelled);
    framer_.feed(bytes);
}
Result<CompletedResponse> ResponseStream::finish() {
    return capture([&] {
        require(!closed_, "stream_closed"); closed_ = true;
        require(!stop_.stop_requested(), "stream_cancelled", Stage::admission, ErrorCode::cancelled);
        framer_.finish(); return reducer_.seal();
    });
}
Request continuation(const CompletedResponse& response, const std::vector<ToolResult>& results) {
    require(results.size() == response.calls().size(), "tool_result_count", Stage::request);
    auto input = response.request().input();
    // Keep these native items in their original order, before any function outputs.
    for (const auto& item : response.items()) input.push_back(item);
    std::set<std::string> operations;
    for (std::size_t i = 0; i < results.size(); ++i) {
        const auto& result = results[i];
        require(result.call_id == response.calls()[i].call_id && !result.operation_id.empty() &&
                operations.insert(result.operation_id).second, "tool_result_identity", Stage::request);
        require(result.output.size() <= 128 * 1024, "tool_output_limit", Stage::request, ErrorCode::limit);
        require(result.effects != Effects::unknown, "tool_effect_unknown", Stage::request);
        input.push_back(Json::object({{"type", Json::string("function_call_output")},
            {"call_id", Json::string(result.call_id)}, {"output", Json::string(result.output)}}));
    }
    const auto& previous = response.request();
    return Request(previous.binding(), previous.epoch(), previous.tools(), std::move(input));
}
} // namespace omh
