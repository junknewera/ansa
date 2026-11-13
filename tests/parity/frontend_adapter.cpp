// Synthetic-only oracle over the actual C++ parser, wire codec and renderer.
#define main ansa_session_adapter_unused_main
#include "session_adapter.cpp"
#undef main
#include "../../src/cli.cpp"
#include "../../src/agent_cli.cpp"
namespace {
std::string frontend_unhex(std::string_view text) {require(text.size()%2==0,"fixture_hex");std::string result;auto nibble=[](char c){if(c>='0'&&c<='9')return c-'0';if(c>='a'&&c<='f')return c-'a'+10;fail(ErrorCode::contract,Stage::frontend,"fixture_hex");};for(std::size_t i=0;i<text.size();i+=2)result.push_back(static_cast<char>(16*nibble(text[i])+nibble(text[i+1])));return result;}
Json frontend_command(const RuntimeCommand& c) {
    std::vector<std::pair<std::string,Json>> f{{"schema_version",num(1)},{"session",str(c.session.value)},{"turn",str(c.turn.value)},{"incarnation",str(c.incarnation.value)},{"command_id",str(c.command.value)}};
    if(const auto* p=std::get_if<Submit>(&c.payload)) {f.emplace_back("type",str("submit"));f.emplace_back("text",str(p->text));f.emplace_back("mode",str(p->mode==AgentMode::plan?"plan":"basic"));}
    else if(const auto* p=std::get_if<Cancel>(&c.payload)){f.emplace_back("type",str("cancel"));f.emplace_back("target",str(p->target.value));}
    else if(const auto* p=std::get_if<Resume>(&c.payload)){f.emplace_back("type",str("resume"));f.emplace_back("target",str(p->target.value));}
    else if(const auto* p=std::get_if<GetSnapshot>(&c.payload)){f.emplace_back("type",str("snapshot"));f.emplace_back("after_sequence",number(p->after_sequence));}
    else if(const auto* p=std::get_if<GetTranscript>(&c.payload)){f.emplace_back("type",str("transcript"));f.emplace_back("before",num(p->before));f.emplace_back("limit",num(p->limit));f.emplace_back("forward",boolean(p->forward));}
    return Json::object(f);
}
class Stdio {
    local::Fd input_{::fcntl(0,F_DUPFD_CLOEXEC,3)},output_{::fcntl(1,F_DUPFD_CLOEXEC,3)};
public:
    explicit Stdio(int fd){require(input_.get()>=0 && output_.get()>=0 && ::dup2(fd,0)>=0 && ::dup2(fd,1)>=0,"fixture_stdio");}
    ~Stdio(){(void)::dup2(input_.get(),0);(void)::dup2(output_.get(),1);}
};
Json frontend_render(const Json& row) {
    local::Fd master(::posix_openpt(O_RDWR|O_NOCTTY|O_CLOEXEC));require(master.get()>=0 && ::grantpt(master.get())==0 && ::unlockpt(master.get())==0,"fixture_pty");
    local::Fd slave(::open(::ptsname(master.get()),O_RDWR|O_NOCTTY|O_CLOEXEC));require(slave.get()>=0,"fixture_pty");
    Stdio stdio(slave.get());
    if(row.at("color")==boolean(true)) (void)::unsetenv("NO_COLOR");else (void)::setenv("NO_COLOR","1",1);(void)::setenv("TERM","xterm",1);
    std::mutex mutex;std::condition_variable cv;std::string bytes;std::size_t previous=0,current=0;bool complete=false;
    Terminal terminal([&](std::string_view part){std::lock_guard lock(mutex);bytes+=part;if(part.find("\033[1A\r")!=std::string::npos){current=1+static_cast<std::size_t>(std::count(part.begin(),part.end(),'\n'));complete=previous<=current;}else if(part.starts_with("\0337"))complete=true;cv.notify_one();return true;});
    std::vector<Json> frames;
    for(const auto& step:row.at("steps").elements()) {
        winsize size{};size.ws_col=static_cast<unsigned short>(step.index("cols"));size.ws_row=static_cast<unsigned short>(step.index("rows"));require(::ioctl(slave.get(),TIOCSWINSZ,&size)==0,"fixture_pty_size");
        std::vector<TerminalEntry> history;for(const auto& e:step.at("entries").elements())history.push_back({e.text("role"),e.text("text")});
        const auto input=frontend_unhex(step.text("input_hex"));
        {std::lock_guard lock(mutex);bytes.clear();complete=false;}
        const auto limit=terminal.render(history,input,step.index("cursor"),step.text("status"),step.at("expanded")==boolean(true),step.index("scroll"),step.text("context"));
        if(step.index("rows")<5 || step.index("cols")<5) {frames.push_back(Json::object({{"frame",str("")},{"scroll_limit",num(0)}}));continue;}
        std::unique_lock lock(mutex);require(cv.wait_for(lock,std::chrono::seconds(2),[&]{return complete;}),"fixture_frame_timeout");
        frames.push_back(Json::object({{"frame",str(bytes)},{"scroll_limit",num(limit)}}));previous=current;
    }
    return Json::array(frames);
}
Json frontend_run(const Json& row) {
    const auto root=fixture_root(row);const auto op=row.text("op");
    if(op=="events") {
        const PreviewText preview{3,"draft","discarded"};const ToolStarted started{{"operation"},"call","safe"};const ToolFinished finished{{"operation"},"call",Execution::failed,Effects::known_result,std::make_shared<const std::string>("result")};
        FrontendSnapshot view{};view.through_sequence=7;view.state="paused";view.request=RequestId{"request"};view.completed_tools=1;view.text="final";view.diagnostic="failure";view.recovery_operation=OperationId{"operation"};view.submit=CommandId{"submit"};view.command=CommandId{"command"};view.tools={started,finished};view.history_items=5;view.compactions=2;view.durable=true;view.resume_available=true;view.mode=AgentMode::plan;view.transcript_size=1;view.preview=preview;
        TranscriptPage page{0,1,1,{{{"turn"},{"request"},"user","question","pending",AgentMode::plan}},{0,16,false}};
        std::vector<EventPayload> payloads{view,preview,page,CommandAccepted{{"command"},RequestId{"request"}},CommandReplayed{{"command"},"completed"},TextDelta{"final"},started,finished,Cancelled{{"command"}},Done{},RuntimeError{std::nullopt,"failure"},RecoveryRequired{{"operation"},Effects::unknown},ResyncRequired{3}};
        std::vector<Json> result;for(auto& payload:payloads)result.push_back(encode_event({1,7,{"session"},{"turn"},{"incarnation"},RequestId{"request"},std::move(payload)}));return Json::array(result);
    }
    if(op=="decode"){auto value=decode_command(row.text("payload"));validate(value);return frontend_command(value);}
    if(op=="render")return frontend_render(row);
    if(op=="args") {
        const auto home=root/"home";std::filesystem::create_directories(home/"workspace");
        (void)::setenv("HOME",home.c_str(),1);(void)::setenv("XDG_STATE_HOME",(root/"xdg").c_str(),1);
        const auto old=std::filesystem::current_path();struct Restore{std::filesystem::path old;~Restore(){std::filesystem::current_path(old);}}restore{old};std::filesystem::current_path(home/"workspace");
        std::vector<std::string> args{"ansa"};for(const auto& arg:row.at("args").elements())args.push_back(arg.text());std::vector<char*> argv;for(auto& arg:args)argv.push_back(arg.data());
        const auto value=parse(static_cast<int>(args.size()),argv.data());
        return Json::object({{"offline",boolean(value.offline)},{"resume",boolean(value.resume)},{"print_config",boolean(value.print_config)},{"cwd",str(value.cwd.string())},{"auth",str(value.auth.string())},{"session",str(value.session)},{"model",value.model?str(*value.model):Json::parse("null")},{"returned_model",value.returned_model?str(*value.returned_model):Json::parse("null")},{"prompt",value.prompt?str(*value.prompt):Json::parse("null")}});
    }
    fail(ErrorCode::contract,Stage::frontend,"frontend_adapter_operation");
}
}
int main(){std::string line;while(std::getline(std::cin,line)){if(line.size()>128*1024)return 2;try{const auto value=frontend_run(Json::parse(line));std::cout<<Json::object({{"accepted",boolean(true)},{"value",value}}).dump()<<'\n';}catch(const Error& e){std::cout<<Json::object({{"accepted",boolean(false)},{"error",diagnostic_json(e)}}).dump()<<'\n';}catch(...){std::cout<<"{\"accepted\":false,\"diagnostic\":\"frontend_adapter_error\"}\n";}std::cout.flush();}return 0;}
