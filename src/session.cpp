#include "omh/session.hpp"
#include "omh/transport.hpp"

namespace omh {
namespace {
template<class T> T unwrap(Result<T> result) {
    if (auto* error = std::get_if<Error>(&result)) throw *error;
    return std::get<T>(std::move(result));
}
Json number(std::size_t value) { return Json::parse(std::to_string(value)); }
bool same_request(const Request& left, const Request& right) {
    return save_request(left) == save_request(right);
}
std::vector<Json> input_for(const std::vector<Json>& history, const std::string& text) {
    auto input = history;
    input.push_back(Json::object({{"role", Json::string("user")}, {"content", Json::string(text)}}));
    return input;
}
} // namespace

Json save_result(const ToolResult& result) {
    return Json::object({{"operation", Json::string(result.operation_id)}, {"call", Json::string(result.call_id)},
        {"output", Json::string(result.output)}, {"execution", number(static_cast<std::size_t>(result.execution))},
        {"effects", number(static_cast<std::size_t>(result.effects))},
        {"dispatched", Json::parse(result.dispatched ? "true" : "false")}});
}

ToolResult load_result(const Json& value) {
    const auto execution = value.index("execution"), effects = value.index("effects");
    require(execution <= static_cast<std::size_t>(Execution::denied) &&
            effects <= static_cast<std::size_t>(Effects::unknown), "session_outcome");
    auto output = value.text("output");
    require(output.size() <= 128 * 1024, "session_outcome_limit");
    require(value.at("dispatched").is_boolean(), "session_outcome_dispatch");
    const bool dispatched = value.at("dispatched") == Json::parse("true");
    require(dispatched || (effects == static_cast<std::size_t>(Effects::not_started) &&
                          execution != static_cast<std::size_t>(Execution::succeeded)), "session_outcome_dispatch");
    return {value.text("operation"), value.text("call"), std::move(output),
            static_cast<Execution>(execution), static_cast<Effects>(effects), dispatched};
}

SessionState::SessionState(SessionId id, Request config, ToolAdvertisement plan_tools)
    : plan_tools_(std::move(plan_tools)), session(std::move(id)), configuration(std::move(config)), epoch(configuration.epoch()) {}
Request SessionState::request_for(const std::string& text, AgentMode mode) const {
    if (mode == AgentMode::basic) return Request(configuration.binding(), epoch, configuration.tools(), input_for(history, text));
    require(mode == AgentMode::plan && !plan_tools_.revision.empty(), "session_plan_tools");
    const auto provenance = Json::object({{"base_epoch", Json::string(epoch.id)}, {"base_state", epoch.exact_state}, {"base_instructions", Json::string(epoch.instructions)},
        {"mode", Json::string("plan")}, {"tools_revision", Json::string(plan_tools_.revision)}});
    ContextEpoch plan{sha256(provenance.dump()), epoch.instructions +
        "\nPlan mode: inspect and reason using read-only tools. Do not modify files or execute commands. Provide a concrete plan.", provenance};
    return Request(configuration.binding(), std::move(plan), plan_tools_, input_for(history, text));
}
void TranscriptIndex::append(TranscriptEntry entry) {
    auto block = std::make_shared<Block>();
    if (tail_ && tail_->entries.size() < 64) {
        block->previous = tail_->previous;
        block->entries = tail_->entries;
    } else block->previous = tail_;
    block->entries.push_back(std::make_shared<const TranscriptEntry>(std::move(entry)));
    tail_ = std::move(block);
    ++size_;
}
const TranscriptEntry& TranscriptIndex::back() const { return *tail_->entries.back(); }
void TranscriptIndex::set_last_state(const std::string& state) {
    auto block = std::make_shared<Block>(*tail_);
    auto entry = std::make_shared<TranscriptEntry>(*block->entries.back());
    entry->state = state;
    block->entries.back() = std::move(entry);
    tail_ = std::move(block);
}
TranscriptPage TranscriptIndex::page(GetTranscript query) const {
    if (query.forward) {
        const auto begin = std::min(query.before, size_);
        const auto limit = std::min<std::size_t>(query.limit, 64);
        const auto max_end = begin + std::min(limit, size_ - begin);
        std::vector<std::pair<std::size_t, std::shared_ptr<const Block>>> blocks;
        auto cursor = size_;
        for (auto block = tail_; block && cursor > begin; block = block->previous) {
            const auto block_begin = cursor - block->entries.size();
            if (block_begin < max_end) blocks.emplace_back(block_begin, block);
            cursor = block_begin;
        }
        std::reverse(blocks.begin(), blocks.end());
        TranscriptPage page{begin, begin, size_, {}, query};
        std::size_t bytes = 0;
        for (const auto& [block_begin, block] : blocks) {
            const auto first = std::max(begin, block_begin) - block_begin;
            for (auto i = first; i < block->entries.size() && page.end < max_end; ++i) {
                const auto& entry = *block->entries[i];
                if (!page.entries.empty() && bytes + entry.text.size() > 256 * 1024) return page;
                bytes += entry.text.size();
                page.entries.push_back(entry);
                ++page.end;
            }
        }
        return page;
    }
    const auto end = query.before == 0 ? size_ : std::min(query.before, size_);
    auto cursor = size_;
    auto block = tail_;
    while (block && cursor - block->entries.size() >= end) {
        cursor -= block->entries.size();
        block = block->previous;
    }
    auto begin = end;
    std::size_t bytes = 0;
    const auto limit = std::min<std::size_t>(query.limit, 64);
    std::vector<TranscriptEntry> reversed;
    while (block && begin > 0 && reversed.size() < limit) {
        const auto block_begin = cursor - block->entries.size();
        while (begin > block_begin && reversed.size() < limit) {
            const auto& entry = *block->entries[begin - block_begin - 1];
            if (!reversed.empty() && bytes + entry.text.size() > 256 * 1024) goto complete;
            bytes += entry.text.size();
            reversed.push_back(entry);
            --begin;
        }
        cursor = block_begin;
        block = block->previous;
    }
complete:
    std::reverse(reversed.begin(), reversed.end());
    return {begin, end, size_, std::move(reversed), query};
}
TranscriptPage SessionState::transcript_page(GetTranscript query) const { return transcript_->page(query); }
Json SessionState::initialization() const {
    return Json::object({{"kind", Json::string("init")}, {"version", number(1)},
        {"session", Json::string(session.value)}, {"configuration", save_request(configuration)}});
}
void SessionState::apply(const Json& event) {
    auto next = *this;
    next.reduce(event);
    *this = std::move(next);
}
std::optional<OperationId> SessionState::unknown_operation() const {
    if (!turn) return {};
    if (turn->intent) return OperationId{turn->intent->operation_id};
    for (const auto& result : turn->results)
        if (result.effects == Effects::unknown) return OperationId{result.operation_id};
    return {};
}

void SessionState::reduce(const Json& event) {
    const auto kind = event.text("kind");
    if (kind == "init") {
        require(!initialized && event.index("version") == 1 && event.text("session") == session.value &&
                same_request(load_request(event.at("configuration")), configuration),
                "session_configuration_mismatch", Stage::frontend, ErrorCode::contract);
        initialized = true;
        return;
    }
    require(initialized, "session_missing_init");
    if (kind == "accepted") {
        require(!turn || turn->stage == "completed" || turn->stage == "cancelled", "session_active_turn");
        const auto id = event.text("command"), turn_id = event.text("turn"), request_id = event.text("request");
        const auto text = event.text("text");
        require(valid_id(id) && valid_id(turn_id) && valid_id(request_id) && !text.empty() &&
                text.size() <= max_submit_bytes && !submits.contains(id) &&
                submits.size() < 65536 && event.index("request_number") == next_request + 1,
                "session_submit");
        auto request = load_request(event.at("next"));
        const auto mode_text = event.has("mode") ? event.text("mode") : "basic";
        require(mode_text == "basic" || mode_text == "plan", "session_mode");
        const auto mode = mode_text == "plan" ? AgentMode::plan : AgentMode::basic;
        auto expected = request_for(text, mode);
        require(same_request(request, expected), "session_request_conflict");
        const auto stage = event.at("compact") == Json::parse("true") ? "compacting" : "generating";
        SubmitReceipt receipt{{id}, {turn_id}, {request_id}, std::make_shared<const std::string>(text), stage, mode};
        const auto run = event.text("operation_run");
        OperationIds validate_run(run);
        turn.emplace(SavedTurn{receipt, run, std::move(request), {}, {}, {}, {}, stage, {}, false, {}});
        turn->round_limit = event.has("round_limit") ? event.index("round_limit") : 1;
        turn->call_limit = event.has("call_limit") ? event.index("call_limit") : 16;
        require(turn->round_limit > 0 && turn->round_limit <= 64 &&
                turn->call_limit > 0 && turn->call_limit <= 256, "session_turn_limits");
        auto entries = std::make_shared<TranscriptIndex>(*transcript_);
        entries->append({{turn_id}, {request_id}, "user", text, stage, mode});
        transcript_ = std::move(entries);
        submits.emplace(id, std::move(receipt));
        ++next_request;
        return;
    }
    require(turn && event.text("turn") == turn->receipt.turn.value, "session_turn_identity");
    auto& current = *turn;
    if (kind == "response") {
        require(current.stage == "generating", "session_response_stage");
        auto response = load_response(event.at("response"));
        auto expected = current.next;
        if (current.tools_response && !current.tools_response->calls().empty()) {
            require(current.results.size() == current.tools_response->calls().size() && !current.intent,
                    "session_results_incomplete");
            expected = continuation(*current.tools_response, current.results);
        }
        require(same_request(response.request(), expected), "session_response_request");
        const auto completed = current.operation_offset + current.results.size();
        if (!response.calls().empty())
            require(current.rounds < 64 && current.rounds - current.resume_round < current.round_limit &&
                    completed <= 256 && response.calls().size() <= 256 - completed &&
                    completed - current.resume_operation <= current.call_limit &&
                    response.calls().size() <= current.call_limit - (completed - current.resume_operation),
                    "session_turn_limits");
        if (current.tools_response) {
            // Commit the previous round only after validating exact continuation.
            for (const auto& result : current.results)
                require(result.dispatched && result.effects == Effects::known_result, "session_round_incomplete");
            current.previous_results.insert(current.previous_results.end(), current.results.begin(), current.results.end());
            current.operation_offset = completed;
            current.results.clear();
            current.tools_response.reset();
        }
        if (!response.calls().empty()) {
            ++current.rounds;
            current.tools_response = response;
            current.stage = "tools";
        } else current.stage = "final";
        current.next = response.request();
        current.response = std::move(response);
    } else if (kind == "intent") {
        require(current.stage == "tools" && current.tools_response && !current.intent &&
                current.results.size() < current.tools_response->calls().size(), "session_intent_stage");
        auto invocation = load_invocation(event.at("invocation"));
        const auto index = current.results.size();
        const auto& call = current.tools_response->calls()[index];
        const auto& descriptors = current.next.tools().descriptors;
        const auto descriptor = std::find_if(descriptors.begin(), descriptors.end(),
            [&](const ToolDescriptor& d) { return d.name == invocation.name; });
        require(invocation.operation_id == current.operation_run + ":op:" + std::to_string(current.operation_offset + index + 1) &&
                invocation.call_id == call.call_id && invocation.name == call.name &&
                invocation.arguments == Json::parse(call.arguments) && descriptor != descriptors.end() &&
                descriptor->implementation_revision == invocation.implementation_revision, "session_intent_conflict");
        current.intent = std::move(invocation);
    } else if (kind == "outcome") {
        require(current.intent.has_value(), "session_outcome_without_intent");
        auto result = load_result(event.at("result"));
        require(result.operation_id == current.intent->operation_id && result.call_id == current.intent->call_id &&
                (result.execution != Execution::succeeded || result.effects == Effects::known_result),
                "session_outcome_identity");
        current.results.push_back(std::move(result));
        current.intent.reset();
    } else if (kind == "continued") {
        require(current.stage == "tools" && current.tools_response && !current.intent &&
                current.results.size() == current.tools_response->calls().size(), "session_continuation");
        for (const auto& result : current.results)
            require(result.dispatched && result.effects == Effects::known_result, "session_tool_failed");
        current.stage = "generating";
    } else if (kind == "compacted") {
        require(current.stage == "compacting", "session_compaction_stage");
        const auto value = event.at("compaction");
        auto compacted = unwrap(CompactedContext::admit(load_request(value.at("request")), value.at("native")));
        const Request expected(configuration.binding(), epoch, configuration.tools(), history);
        require(same_request(compacted.request(), expected), "session_compaction_request");
        require(compacted.output().size() + 1 < compact_item_threshold, "compaction_item_limit");
        epoch = compact_epoch(configuration.epoch(), ++compactions, compacted);
        history = compacted.output();
        history_bytes = Json::array(history).dump().size();
        current.next = request_for(*current.receipt.text, current.receipt.mode);
        current.stage = "generating";
    } else if (kind == "done") {
        require(current.stage == "final" && current.response && current.response->calls().empty(), "session_done_stage");
        auto final_text = response_text(*current.response);
        auto entries = std::make_shared<TranscriptIndex>(*transcript_);
        entries->set_last_state("completed");
        entries->append({current.receipt.turn, current.receipt.request, "assistant", std::move(final_text), "completed", current.receipt.mode});
        transcript_ = std::move(entries);
        history = current.response->request().input();
        for (const auto& item : current.response->items()) history.push_back(item);
        history_bytes = Json::array(history).dump().size();
        current.stage = "completed";
    } else if (kind == "cancel") {
        require(current.stage != "completed", "session_cancel_completed");
        current.stage = "cancelled";
        current.paused = false;
    } else if (kind == "pause") {
        require(current.stage != "completed" && current.stage != "cancelled", "session_pause_terminal");
        current.diagnostic = event.text("diagnostic");
        require(valid_id(current.diagnostic), "session_diagnostic");
        current.paused = true;
    } else if (kind == "resume") {
        const auto id = event.text("command");
        require(current.stage != "completed" && current.stage != "cancelled" && !unknown_operation() &&
                valid_id(id) && !current.resumes.contains(id) && current.resumes.size() < 4096,
                "session_resume");
        current.resumes.insert(id);
        current.resume_round = current.rounds;
        while (!current.results.empty() && !current.results.back().dispatched &&
               current.results.back().effects == Effects::not_started)
            current.results.pop_back();
        current.resume_operation = current.operation_offset + current.results.size();
        current.paused = false;
        current.diagnostic.clear();
    } else fail(ErrorCode::contract, Stage::frontend, "session_record_kind");
    current.receipt.state = current.paused ? "paused" : current.stage;
    if (kind != "done" && transcript_->back().state != current.receipt.state) {
        auto entries = std::make_shared<TranscriptIndex>(*transcript_);
        entries->set_last_state(current.receipt.state);
        transcript_ = std::move(entries);
    }
    submits.at(current.receipt.command.value).state = current.receipt.state;
}
} // namespace omh
