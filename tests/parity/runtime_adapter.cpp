// Reuse the reviewed synthetic root guard and session snapshot codecs.
#define main ansa_session_adapter_unused_main
#include "session_adapter.cpp"
#undef main
#include "omh/runtime.hpp"
#include <atomic>
#include <mutex>
namespace {
const char* execution_name(Execution value) {switch(value){case Execution::succeeded:return "succeeded";case Execution::failed:return "failed";case Execution::cancelled:return "cancelled";case Execution::denied:return "denied";}throw std::runtime_error("execution");}
const char* effects_name(Effects value) {switch(value){case Effects::known_result:return "known_result";case Effects::not_started:return "not_started";case Effects::unknown:return "unknown";}throw std::runtime_error("effects");}
Json runtime_event(const RuntimeEvent& e) {
    validate(e);
    std::vector<std::pair<std::string,Json>> f{{"sequence",num(e.stream_sequence)},
        {"session",str(e.session.value)},{"turn",str(e.turn.value)},
        {"incarnation",str(e.incarnation.value)},{"request",e.request?str(e.request->value):Json::parse("null")}};
    auto field=[&](const char* key,Json value){f.emplace_back(key,std::move(value));};
    if(const auto* p=std::get_if<CommandAccepted>(&e.payload)){field("type",str("accepted"));field("command",str(p->command.value));}
    else if(const auto* p=std::get_if<ToolStarted>(&e.payload)){field("type",str("started"));field("operation",str(p->operation.value));field("call",str(p->call_id));field("name",str(p->name));}
    else if(const auto* p=std::get_if<ToolFinished>(&e.payload)){field("type",str("finished"));field("operation",str(p->operation.value));field("call",str(p->call_id));field("execution",str(execution_name(p->execution)));field("effects",str(effects_name(p->effects)));field("output",str(*p->output));}
    else if(const auto* p=std::get_if<TextDelta>(&e.payload)){field("type",str("text"));field("text",str(p->text));}
    else if(std::holds_alternative<Done>(e.payload))field("type",str("done"));
    else if(const auto* p=std::get_if<RuntimeError>(&e.payload)){field("type",str("error"));field("diagnostic",str(p->diagnostic));}
    else if(const auto* p=std::get_if<FrontendSnapshot>(&e.payload)){require(p->through_sequence==e.stream_sequence,"runtime_fixture_cursor");return Json::parse("null");}
    else if(std::holds_alternative<TranscriptPage>(e.payload))return Json::parse("null");
    else throw std::runtime_error("unexpected runtime trace event");
    return Json::object(f);
}
CompletedResponse runtime_response(const Request& request,std::size_t calls) {
    const bool final=!request.input().empty() && request.input().back().has("type") && request.input().back().text("type")=="function_call_output";
    std::vector<Json> items;
    if(final || calls==0) items.push_back(Json::object({{"type",str("message")},{"id",str("message")},{"role",str("assistant")},{"status",str("completed")},
        {"content",Json::array({Json::object({{"type",str("output_text")},{"text",str(final?"OMH_TOOL_OK:OMH_FIXED_VALUE_42":"ANSWER")}})})}}));
    else for(std::size_t i=0;i<calls;++i)items.push_back(Json::object({{"type",str("function_call")},{"id",str("item"+std::to_string(i))},{"call_id",str("call"+std::to_string(i))},
        {"namespace",str("omh")},{"name",str("get_test_value")},{"arguments",str("{}")},{"status",str("completed")}}));
    return load_response(Json::object({{"request",save_request(request)},{"id",str("response")},{"model",str(request.binding().returned_model)},{"items",Json::array(items)}}));
}
Json runtime_run(const Json& row) {
    require(row.text("op")=="trace","runtime_adapter_operation");
    const auto calls=row.index("calls"),turns=row.index("turns"),start=row.has("start")?row.index("start"):0;
    require(calls<=3 && turns>=1 && turns<=4 && start<=16,"runtime_fixture_limits");
    const auto mode=row.text("mode");require(mode=="basic" || mode=="plan","runtime_fixture_mode");
    std::atomic<std::size_t> count=0;
    ToolRegistry registry;auto tool=test_value_tool();tool.read_only=true;tool.requires_journal=row.has("root");
    tool.execute=[&](const Json&,std::stop_token){++count;return ToolOutcome{Execution::succeeded,Effects::known_result,R"({"ok":true,"value":"OMH_FIXED_VALUE_42"})"};};
    registry.register_tool(std::move(tool));const auto tools=registry.snapshot();
    const ContextEpoch epoch{"fixture_epoch","Use the pure fixture tool",Json::parse(R"({"mode":"offline","revision":1})")};
    SessionOptions session; if(row.has("root")){session.directory=fixture_root(row)/"journal";session.journal=options(row);}
    std::vector<Json> requests,events;std::mutex request_mutex;
    auto generate=[&](const Request& request,std::stop_token)->Result<CompletedResponse>{std::lock_guard lock(request_mutex);requests.push_back(save_request(request));return runtime_response(request,calls);};
    std::optional<FrontendSnapshot> final;
    Json transcript=Json::parse("null");
    std::string incarnation;
    {
        Runtime runtime({"offline"},epoch,tools,generate,{},session);incarnation=runtime.incarnation().value;
        auto post=[&](std::string id,CommandPayload payload,std::string turn){require(unwrap(runtime.post({1,{"offline"},{turn},runtime.incarnation(),{id},std::move(payload)}))==PostStatus::accepted,"runtime_fixture_post");};
        auto consume=[&](const RuntimeEvent& e){auto value=runtime_event(e);if(value!=Json::parse("null"))events.push_back(std::move(value));if(const auto* p=std::get_if<FrontendSnapshot>(&e.payload))final=*p;};
        for(std::size_t i=start;i<start+turns;++i){const auto id="turn"+std::to_string(i);post(id,Submit{"fixture",mode=="plan"?AgentMode::plan:AgentMode::basic},id);
            bool done=false;while(const auto e=runtime.next_event()){consume(*e);if(std::holds_alternative<RuntimeError>(e->payload))throw std::runtime_error("runtime trace failed");if(std::holds_alternative<Done>(e->payload)){done=true;break;}}require(done,"runtime_fixture_done");}
        post("query",GetTranscript{0,64,false},"query");
        while(const auto e=runtime.next_event()){consume(*e);if(const auto* p=std::get_if<TranscriptPage>(&e->payload)){std::vector<Json> entries;for(const auto& x:p->entries)entries.push_back(Json::object({{"turn",str(x.turn.value)},{"request",str(x.request.value)},{"role",str(x.role)},{"text",str(x.text)},{"state",str(x.state)},{"mode",str(x.mode==AgentMode::plan?"plan":"basic")}}));transcript=Json::array(entries);break;}}
        runtime.close_input();while(const auto e=runtime.next_event())consume(*e);
    }
    require(final.has_value(),"runtime_fixture_snapshot");const auto& f=*final;
    Json context=Json::parse("null");
    if(row.has("root")){SessionState state({"offline"},Request({},epoch,tools->advertisement(),{}),tools->read_only()->advertisement());Journal journal(session.directory,[&](const Json& e){state.apply(e);});require(journal.diagnostic().empty(),"runtime_fixture_reopen");context=Json::array(state.history);}
    return Json::object({{"incarnation",str(incarnation)},{"invocations",num(count)},{"requests",Json::array(requests)},{"events",Json::array(events)},{"transcript",transcript},{"context",context},
        {"snapshot",Json::object({{"state",str(f.state)},{"busy",boolean(f.worker_busy)},{"text",str(f.text)},{"diagnostic",str(f.diagnostic)},{"completed",num(f.completed_tools)},{"history",num(f.history_items)},{"transcript",num(f.transcript_size)},{"compactions",num(f.compactions)},{"durable",boolean(f.durable)},{"storage_failed",boolean(f.storage_failed)},{"mode",str(f.mode==AgentMode::plan?"plan":"basic")},
        {"through",num(f.through_sequence)},{"request",f.request?str(f.request->value):Json::parse("null")},{"submit",f.submit?str(f.submit->value):Json::parse("null")},
        {"recovery",f.recovery_operation?str(f.recovery_operation->value):Json::parse("null")},{"preview",f.preview?str(f.preview->text):Json::parse("null")},{"resume_available",boolean(f.resume_available)}})}});
}
}
int main(){std::string line;while(std::getline(std::cin,line)){if(line.size()>4096)return 2;try{std::cout<<Json::object({{"accepted",boolean(true)},{"value",runtime_run(Json::parse(line))}}).dump()<<'\n';}catch(const std::exception& e){std::cerr<<e.what()<<'\n';return 1;}}return 0;}
