// Voiceless bridge: lets the voiceless binary commit recognized text into
// the focused input field through Fcitx5 instead of synthesizing keys
// (wtype) or pasting (clipboard), and show the live partial as preedit
// (underlined, uncommitted text) in that field while the user speaks.
// Exposed on Fcitx5's own session-bus name (org.fcitx.Fcitx5) at
// /voiceless, interface dev.voiceless.Fcitx5Bridge1.

#include <string>
#include <tuple>

#include <dbus_public.h>
#include <fcitx-utils/capabilityflags.h>
#include <fcitx-utils/dbus/bus.h>
#include <fcitx-utils/dbus/objectvtable.h>
#include <fcitx-utils/trackableobject.h>
#include <fcitx/addonfactory.h>
#include <fcitx/addoninstance.h>
#include <fcitx/addonmanager.h>
#include <fcitx/inputcontext.h>
#include <fcitx/inputpanel.h>
#include <fcitx/instance.h>
#include <fcitx/text.h>
#include <fcitx/userinterface.h>

namespace {

constexpr char kPath[] = "/voiceless";
constexpr char kInterface[] = "dev.voiceless.Fcitx5Bridge1";

class BridgeObject : public fcitx::dbus::ObjectVTable<BridgeObject> {
public:
    explicit BridgeObject(fcitx::Instance *instance) : instance_(instance) {}

    // Commit `text` into the focused input context, replacing any preedit
    // this bridge showed. Returns (ok, detail): detail is the target program
    // on success, the reason on failure.
    std::tuple<bool, std::string> commitString(const std::string &text) {
        clearPreedit();
        if (text.empty()) {
            return {false, "empty text"};
        }
        auto *ic = focusedInputContext();
        if (!ic) {
            return {false, "no focused input context"};
        }
        ic->commitString(text);
        return {true, describe(ic)};
    }

    // Show `text` as underlined preedit in the focused input context:
    // inline in clients that render preedit, in the Fcitx5 panel otherwise.
    // Focus moving elsewhere clears it from the old context; empty text
    // clears it.
    std::tuple<bool, std::string> setPreedit(const std::string &text) {
        auto *ic = focusedInputContext();
        if (!ic || ic != preeditContext_.get()) {
            clearPreedit();
        }
        if (!ic) {
            return {false, "no focused input context"};
        }
        if (text.empty()) {
            return {true, describe(ic)};
        }
        fcitx::Text preedit(text, fcitx::TextFormatFlag::Underline);
        preedit.setCursor(static_cast<int>(text.size()));
        if (ic->capabilityFlags().test(fcitx::CapabilityFlag::Preedit)) {
            ic->inputPanel().setClientPreedit(preedit);
        } else {
            ic->inputPanel().setPreedit(preedit);
        }
        refresh(ic);
        preeditContext_ = ic->watch();
        return {true, describe(ic)};
    }

    std::tuple<bool, std::string> clearPreeditMethod() {
        clearPreedit();
        return {true, ""};
    }

    // Report whether a focused input context exists, without side effects.
    std::tuple<bool, std::string> status() {
        auto *ic = focusedInputContext();
        if (!ic) {
            return {false, "no focused input context"};
        }
        return {true, describe(ic)};
    }

private:
    void clearPreedit() {
        if (auto *ic = preeditContext_.get()) {
            ic->inputPanel().setClientPreedit(fcitx::Text());
            ic->inputPanel().setPreedit(fcitx::Text());
            refresh(ic);
        }
        preeditContext_.unwatch();
    }

    static void refresh(fcitx::InputContext *ic) {
        ic->updatePreedit();
        ic->updateUserInterface(fcitx::UserInterfaceComponent::InputPanel);
    }

    fcitx::InputContext *focusedInputContext() {
        auto *ic = instance_->mostRecentInputContext();
        return ic && ic->hasFocus() ? ic : nullptr;
    }

    static std::string describe(fcitx::InputContext *ic) {
        std::string program = ic->program();
        if (program.empty()) {
            program = "unknown";
        }
        return program + " (" + std::string(ic->frontendName()) + ")";
    }

    fcitx::Instance *instance_;
    // The context showing this bridge's preedit, if it still exists.
    fcitx::TrackableObjectReference<fcitx::InputContext> preeditContext_;
    FCITX_OBJECT_VTABLE_METHOD(commitString, "CommitString", "s", "bs");
    FCITX_OBJECT_VTABLE_METHOD(setPreedit, "SetPreedit", "s", "bs");
    FCITX_OBJECT_VTABLE_METHOD(clearPreeditMethod, "ClearPreedit", "", "bs");
    FCITX_OBJECT_VTABLE_METHOD(status, "Status", "", "bs");
};

class VoicelessBridge : public fcitx::AddonInstance {
    fcitx::Instance *instance_;
    FCITX_ADDON_DEPENDENCY_LOADER(dbus, instance_->addonManager());
    BridgeObject object_;

public:
    explicit VoicelessBridge(fcitx::Instance *instance)
        : instance_(instance), object_(instance) {
        auto *bus = dbus()->call<fcitx::IDBusModule::bus>();
        bus->addObjectVTable(kPath, kInterface, object_);
    }
};

class VoicelessBridgeFactory : public fcitx::AddonFactory {
public:
    fcitx::AddonInstance *create(fcitx::AddonManager *manager) override {
        return new VoicelessBridge(manager->instance());
    }
};

} // namespace

FCITX_ADDON_FACTORY(VoicelessBridgeFactory);
