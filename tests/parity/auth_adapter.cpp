// Test-only adapter: production AuthManager and JWT/callback validators.
// Reuse the actual C++ native auth scenarios rather than a second fixture model.
#define main cpp_auth_suite_main
#include "../auth_tests.cpp"
#undef main
#include <fcntl.h>
#include <limits>

namespace {
struct FixtureFd { int fd; ~FixtureFd(){if(fd>=0) ::close(fd);} };
Json fixture_credentials(const std::filesystem::path& root) {
    FixtureFd directory{::open((root/"auth").c_str(),O_RDONLY|O_DIRECTORY|O_CLOEXEC|O_NOFOLLOW)};
    require(directory.fd>=0,"fixture_store",Stage::frontend);
    FixtureFd file{::openat(directory.fd,"credentials.json",O_RDONLY|O_CLOEXEC|O_NOFOLLOW|O_NONBLOCK)};
    struct stat info{};
    require(file.fd>=0 && ::fstat(file.fd,&info)==0 && S_ISREG(info.st_mode) && info.st_uid==::geteuid() &&
            info.st_nlink==1 && (info.st_mode&077)==0 && info.st_size>0 && info.st_size<=256*1024,"fixture_store",Stage::frontend);
    std::string bytes(static_cast<std::size_t>(info.st_size),'\0'); std::size_t used=0;
    while(used<bytes.size()) {
        const auto count=::read(file.fd,bytes.data()+used,bytes.size()-used);
        if(count<0 && errno==EINTR) continue;
        require(count>0,"fixture_store",Stage::frontend); used+=static_cast<std::size_t>(count);
    }
    return Json::parse(bytes,256*1024);
}
Json flag(bool value) { return Json::parse(value ? "true" : "false"); }
Json status_json(const AuthStatus& s) {
    return Json::object({{"logged_in",flag(s.logged_in)},{"refresh_pending",flag(s.refresh_pending)},
        {"plan_enabled",flag(s.plan_enabled)},{"account",Json::string(s.account)},
        {"expires_in",number(s.expires_in)},{"refresh_wait",number(s.refresh_wait)},{"session",Json::string(s.session)}});
}
void owned_root(const std::filesystem::path& path) {
    const auto owned=[](bool value,const char* code){ require(value,code,Stage::frontend,ErrorCode::validation); };
    owned(path.native().find('\0')==std::string::npos && path==path.lexically_normal() && path.is_absolute() && path.parent_path()==std::filesystem::temp_directory_path() &&
          path.filename().string().starts_with("ansa-m3-fixture-"),"fixture_root");
    const auto directory=::open(path.c_str(),O_RDONLY|O_DIRECTORY|O_CLOEXEC|O_NOFOLLOW);
    owned(directory>=0,"fixture_root");
    struct Close { int fd; ~Close(){ if(fd>=0) ::close(fd); } } dir{directory};
    struct stat info{};
    owned(::fstat(directory,&info)==0 && info.st_uid==::geteuid() && (info.st_mode&077)==0,"fixture_root");
    const auto fd=::openat(directory,"fixture-owner",O_RDONLY|O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC);
    owned(fd>=0,"fixture_owner"); Close marker{fd};
    owned(::fstat(fd,&info)==0 && S_ISREG(info.st_mode) && info.st_uid==::geteuid() && info.st_nlink==1 && (info.st_mode&077)==0 && info.st_size==21,"fixture_owner");
    char value[22]{}; const auto count=::read(fd,value,sizeof(value));
    owned(count==21 && std::string_view(value,21)=="ansa-m3-synthetic-v1\n","fixture_owner");
}
void event_log(const std::filesystem::path& root, std::string_view event) {
    FixtureFd dir{::open(root.c_str(),O_RDONLY|O_DIRECTORY|O_CLOEXEC|O_NOFOLLOW)};
    require(dir.fd>=0,"fixture_events",Stage::frontend);
    FixtureFd file{::openat(dir.fd,"exchanges.log",O_WRONLY|O_APPEND|O_CREAT|O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC,0600)};
    struct stat status{};
    require(file.fd>=0 && ::fstat(file.fd,&status)==0 && S_ISREG(status.st_mode) && status.st_uid==::geteuid() &&
        status.st_nlink==1 && (status.st_mode&077)==0 && status.st_size<=1024*1024,"fixture_events",Stage::frontend);
    const auto value=std::string(event)+"\n";
    require(::write(file.fd,value.data(),value.size())==static_cast<ssize_t>(value.size()) && ::fsync(file.fd)==0,"fixture_events",Stage::frontend);
}
Json store_action(const Json& request) {
    const auto root=std::filesystem::path(request.text("root")); owned_root(root);
    const auto action=request.text("action"); Mock mock;
    if(request.has("now")) mock.now=request.index("now");
    if(request.has("permission")) mock.permission=request.at("permission")==flag(true);
    if(request.has("revoke_failure")) mock.revoke_failure=request.at("revoke_failure")==flag(true);
    unsigned fault_count=0;
    AuthStoreObserver observer;
    if(request.has("fault")) {
        const auto fault=request.at("fault");
        const auto wanted=fault.text("point"),name=fault.text("name");
        const auto nth=fault.index("nth");
        require(nth>0 && nth<=8 && (name=="registration.json" || name=="credentials.json" || name=="signed-out"),"fixture_fault",Stage::frontend);
        require(wanted=="temporary_synced" || wanted=="replaced" || wanted=="tokens_removed" || wanted=="logout_marked","fixture_fault",Stage::frontend);
        observer=[&,wanted,name,nth](AuthStorePoint point,std::string_view file) {
            const std::string_view actual=point==AuthStorePoint::temporary_synced ? "temporary_synced" : point==AuthStorePoint::replaced ? "replaced" :
                point==AuthStorePoint::tokens_removed ? "tokens_removed" : point==AuthStorePoint::logout_marked ? "logout_marked" : "before_dispatch";
            if(actual==wanted && file==name && ++fault_count==nth) ::_exit(71);
        };
    }
    AuthManager manager(root/"auth",[&](const HttpRequest& r,std::stop_token stop) {
        if(r.url.ends_with("/token")) {
            const bool refresh=form(r.body).at("grant_type")=="refresh_token";
            if(refresh) {
                const auto record=fixture_credentials(root);
                require(record.at("refresh_pending")==flag(true) && form(r.body).at("refresh_token")==record.text("refresh_token"),"fixture_refresh_intent",Stage::frontend);
                mock.nonce=record.text("nonce");
            }
            event_log(root,refresh ? "refresh" : "code");
        }
        else if(r.url.ends_with("/revoke")) event_log(root,"revoke");
        return mock.exchange(r,stop);
    },[&]{return mock.now;},observer);
    std::vector<std::pair<std::string,Json>> out{{"accepted",flag(true)}};
    if(action=="login") mock.login(manager);
    else if(action=="access" || action=="refresh") {
        const auto lease=manager.access({},action=="refresh");
        out.emplace_back("account",Json::string(lease.account)); out.emplace_back("session",Json::string(lease.session));
        out.emplace_back("expires_at",number(lease.expires_at)); out.emplace_back("bearer_present",flag(!lease.bearer.empty()));
    } else if(action=="logout") out.emplace_back("confirmed",flag(manager.logout()));
    else check(action=="status","fixture_store_action");
    out.emplace_back("status",status_json(manager.status()));
    return Json::object(out);
}
Json evaluate(const Json& request) {
    const auto op=request.text("op");
    if(op=="jwt") {
        const auto claims=verify_id_token(request.text("jwt"),request.at("jwks"),request.text("client"),request.text("nonce"),
            request.index("now"),request.has("refresh") && request.at("refresh")==flag(true));
        return Json::object({{"accepted",flag(true)},{"subject",claims.at("sub")},{"claims_sha256",Json::string(sha256(claims.dump()))}});
    }
    if(op=="generate") {
        const auto bits=request.has("bits") ? request.index("bits") : 2048;
        check(bits>=512 && bits<=8192,"fixture_key_bits");
        const auto exponent=request.has("exponent") ? request.text("exponent") : "65537";
        check(exponent.size()<=20,"fixture_exponent");
        static std::map<std::pair<int,std::string>,std::unique_ptr<Key>> fixture_keys;
        auto& key=fixture_keys[{static_cast<int>(bits),exponent}];
        if(!key) key=std::make_unique<Key>(static_cast<int>(bits),exponent);
        const auto payload=request.has("claims") ? request.at("claims") : claims(1800000000,"nonce");
        const auto header=request.has("header_raw") ? request.text("header_raw") :
            Json::object({{"alg",Json::string(request.has("algorithm") ? request.text("algorithm") : "RS256")},{"kid",Json::string("fixture_key")}}).dump();
        const auto body=request.has("payload_raw") ? request.text("payload_raw") : payload.dump();
        return Json::object({{"jwt",Json::string(key->sign_raw(header,body))},{"jwks",key->jwks()}});
    }
    if(op=="callback") {
        const auto port=request.index("port"); check(port>0 && port<=65535,"fixture_port");
        const auto fields=auth_callback_fields(request.text("request"),request.text("state"),static_cast<std::uint16_t>(port));
        return Json::object({{"accepted",flag(true)},{"field_count",number(fields.keys().size())},{"code_present",flag(fields.has("code"))},
            {"code_sha256",Json::string(sha256(fields.text("code")))}});
    }
    if(op=="pkce") return Json::object({{"accepted",flag(true)},{"challenge",Json::string(pkce_challenge(request.text("verifier")))}});
    if(op=="form") {
        std::vector<std::pair<std::string,std::string>> fields;
        for(const auto& item:request.at("fields").elements()) fields.emplace_back(item.text("key"),item.text("value"));
        return Json::object({{"accepted",flag(true)},{"encoded",Json::string(form_encode(fields))}});
    }
    if(op=="base64") {
        const auto decoded=unbase64url(request.text("encoded"));
        return Json::object({{"accepted",flag(true)},{"encoded",Json::string(base64url(decoded))},{"bytes",number(decoded.size())}});
    }
    if(op=="discovery") {
        (void)AuthEndpoints::discover(request.at("discovery")); return Json::object({{"accepted",flag(true)}});
    }
    if(op=="scenario") {
        const auto name=request.text("case");
        const char* argv[]{"auth-scenario",name.c_str()};
        check(cpp_auth_suite_main(2,const_cast<char**>(argv))==0,"fixture_native_scenario");
        return Json::object({{"passed",flag(true)},{"case",Json::string(name)}});
    }
    if(op=="ancestor") {
        Temp temp; Mock mock; const auto real=temp.root/"real/auth";
        auto manager=mock.manager(real); mock.login(*manager); const auto before=manager->status();
        const auto posts=mock.grants.load()+mock.refreshes.load();
        std::filesystem::create_directory_symlink(temp.root/"real",temp.root/"alias");
        auto aliased=mock.manager(temp.root/"alias/auth"); const auto through=aliased->status();
        const auto after=manager->status();
        return Json::object({{"accepted",flag(through.logged_in && through.account==before.account && through.session==before.session)},
            {"zero_posts",flag(posts==mock.grants.load()+mock.refreshes.load())},
            {"real_path_compatible",flag(after.logged_in && after.account==before.account && after.session==before.session)}});
    }
    if(op=="store") return store_action(request);
    fail(ErrorCode::validation,Stage::frontend,"fixture_unknown_operation");
}
}
int main() {
    std::string line; char byte=0;
    while(std::cin.get(byte)) {
        if(byte!='\n') {
            if(line.size()==4*1024*1024) { std::cerr<<"auth_adapter: oversized input\n"; return 2; }
            line.push_back(byte); continue;
        }
        try {
            check(line.size()<=4*1024*1024,"fixture_line_limit");
            std::cout << evaluate(Json::parse(line,4*1024*1024)).dump() << '\n';
        } catch(const Error& e) {
            std::cout << Json::object({{"accepted",flag(false)},{"diagnostic",Json::string(e.diagnostic)}}).dump() << '\n';
        } catch(...) { std::cout << "{\"adapter_error\":\"fixture_failure\"}\n"; }
        std::cout.flush();
        line.clear();
    }
    if(!line.empty()) { std::cerr<<"auth_adapter: unterminated input\n"; return 2; }
}
