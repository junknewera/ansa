// Reuse the reviewed synthetic fixture-root guard and legacy JSON helpers.
#define main ansa_session_adapter_unused_main
#include "session_adapter.cpp"
#undef main
#include "omh/local_tools.hpp"
#include <csignal>
namespace {
Json tools_outcome(const ToolOutcome& value){return Json::object({{"execution",num(static_cast<std::size_t>(value.execution))},{"effects",num(static_cast<std::size_t>(value.effects))},{"output",str(value.output)}});}
Json tools_run(const Json& row){
    const auto root=fixture_root(row);const auto home=root/"home",cwd=home/"workspace";
    std::filesystem::create_directories(cwd);
    const auto op=row.text("op");
    if(op=="process" || op=="recover"){
        if(row.has("inherit_signals") && row.at("inherit_signals")==boolean(true)){
            struct sigaction ignore{};ignore.sa_handler=SIG_IGN;ignore.sa_flags=SA_NOCLDWAIT;::sigemptyset(&ignore.sa_mask);
            require(::sigaction(SIGCHLD,&ignore,nullptr)==0,"fixture_signal_fault");
            sigset_t mask;::sigemptyset(&mask);::sigaddset(&mask,SIGTERM);require(::sigprocmask(SIG_BLOCK,&mask,nullptr)==0,"fixture_signal_fault");
        }
        ProcessRunner runner(root/"jobs");const auto invocation=row.has("invocation")?load_invocation(row.at("invocation")):ToolInvocation{"operation","call","execute","1",Json::parse("{}")};
        if(op=="recover"){const auto result=runner.recover(invocation);return result?tools_outcome(*result):Json::parse("null");}
        ProcessOptions options{cwd,{},row.has("timeout_ms")?row.index("timeout_ms"):3000,row.has("output_bytes")?row.index("output_bytes"):16384};
        for(const auto& arg:row.at("argv").elements())options.argv.push_back(arg.text());
        if(row.has("cwd"))options.cwd=row.text("cwd");
        return tools_outcome(runner.run(invocation,options,{}));
    }
    const auto settings=load_agent_settings(cwd,home);const auto epoch=agent_epoch(settings);
    const auto tools=local_tools(settings,epoch,root/"session",root/"auth");
    if(op=="config")return Json::object({{"settings",settings.effective()},
        {"epoch",Json::object({{"id",str(epoch.id)},{"instructions",str(epoch.instructions)},{"exact_state",epoch.exact_state}})},
        {"tools",save_request(Request({},epoch,tools->advertisement(),{})).at("tools")},
        {"readonly",save_request(Request({},epoch,tools->read_only()->advertisement(),{})).at("tools")}});
    require(op=="local","tools_adapter_operation");
    auto args=row.at("arguments");
    if(row.has("initial_token") && row.at("initial_token")==boolean(true)){
        std::vector<std::pair<std::string,Json>> fields;for(const auto& key:args.keys())if(key!="instruction_token")fields.emplace_back(key,args.at(key));
        fields.emplace_back("instruction_token",str(sha256(epoch.exact_state.at("instructions").dump())));args=Json::object(fields);
    }
    Request request({},epoch,tools->advertisement(),{});
    const auto item=Json::object({{"type",str("function_call")},{"id",str("item")},{"call_id",str("call")},{"name",row.at("name")},{"namespace",str("omh")},{"arguments",str(args.dump())},{"status",str("completed")}});
    const auto response=load_response(Json::object({{"request",save_request(request)},{"id",str("response")},{"model",str(request.binding().returned_model)},{"items",Json::array({item})}}));
    OperationIds ids("fixture");auto batch=unwrap(PreparedBatch::prepare(response,tools,ids));const auto results=unwrap(batch.execute());
    require(results.size()==1,"tools_result_count");return Json::object({{"execution",num(static_cast<std::size_t>(results[0].execution))},{"effects",num(static_cast<std::size_t>(results[0].effects))},{"output",str(results[0].output)}});
}
}
int main(int argc,char** argv){
    if(argc==3 && std::string_view(argv[1])=="--process-worker")return process_worker(argv[2]);
    std::string line;while(std::getline(std::cin,line)){if(line.size()>128*1024)return 2;try{std::cout<<Json::object({{"accepted",boolean(true)},{"value",tools_run(Json::parse(line))}}).dump()<<'\n';}catch(const Error& e){std::cout<<Json::object({{"accepted",boolean(false)},{"diagnostic",str(e.diagnostic)}}).dump()<<'\n';}catch(...){std::cout<<"{\"accepted\":false,\"diagnostic\":\"tools_adapter_error\"}\n";}}return 0;
}
