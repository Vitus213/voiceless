// Voiceless bridge: lets the voiceless binary commit recognized text into
// the focused input field through Fcitx5 instead of synthesizing keys
// (wtype) or pasting (clipboard), and show the live partial as preedit
// (underlined, uncommitted text) in that field while the user speaks.
// Exposed on Fcitx5's own session-bus name (org.fcitx.Fcitx5) at
// /voiceless, interface dev.voiceless.Fcitx5Bridge1.
//
// D-Bus contract. Every method returns the tuple (b ok, s detail); the
// Rust client (rust/src/fcitx5.rs) deserializes exactly `(bool, String)`
// and `voiceless doctor` calls Status.
//   CommitString(s text) -> (b, s)  Commit text into the focused input
//       context after clearing this bridge's preedit. detail is the target
//       program as "name (frontend)" on success, the reason on failure
//       ("empty text", "no focused input context").
//   SetPreedit(s text) -> (b, s)    Show text underlined as preedit in the
//       focused context; empty text clears it. detail as above.
//   ClearPreedit() -> (b, s)        Remove the preedit; always (true, "").
//   Status() -> (b, s)              (true, program) while a context has
//       focus, (false, "no focused input context") otherwise. No effects.
//   FocusedContext() returns the stable id of the focused input context.
//   CommitToContext(s id, s text) and PreeditToContext(s id, s text) refuse
//       when that context no longer has focus. Use these for dictation.
// Calls are dispatched on Fcitx5's single event loop in bus order, so a
// client that sends SetPreedit then CommitString on one connection sees
// them applied in that order: a stale partial never lands after the final.
//
// Preedit lifecycle. The bridge tracks the context it drew into with a
// reference that becomes null when Fcitx5 destroys the context. Each new
// preview clears the previous context if necessary. A refused preview or
// commit also clears the old preedit, as does an explicit ClearPreedit.
// Fcitx5 loads this module at startup and registers it on the addon bus.

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
        return commit(focusedInputContext(), text);
    }

    std::tuple<bool, std::string> commitToContext(const std::string &id,
                                                 const std::string &text) {
        auto *ic = focusedInputContext();
        if (!ic || contextId(ic) != id) {
            clearPreedit();
            return {false, "the original Fcitx5 input context no longer has focus"};
        }
        return commit(ic, text);
    }

    // Show `text` as underlined preedit in the focused input context:
    // inline in clients that render preedit, in the Fcitx5 panel otherwise.
    // Focus moving elsewhere clears it from the old context; empty text
    // clears it.
    std::tuple<bool, std::string> setPreedit(const std::string &text) {
        return showPreedit(focusedInputContext(), text);
    }

    std::tuple<bool, std::string> preeditToContext(const std::string &id,
                                                  const std::string &text) {
        auto *ic = focusedInputContext();
        if (!ic || contextId(ic) != id) {
            clearPreedit();
            return {false, "the original Fcitx5 input context no longer has focus"};
        }
        return showPreedit(ic, text);
    }

    std::tuple<bool, std::string> focusedContext() {
        auto *ic = focusedInputContext();
        if (!ic) {
            return {false, "no focused input context"};
        }
        return {true, contextId(ic)};
    }

private:
    std::tuple<bool, std::string> commit(fcitx::InputContext *ic, const std::string &text) {
        clearPreedit();
        if (text.empty()) {
            return {false, "empty text"};
        }
        if (!ic) {
            return {false, "no focused input context"};
        }
        ic->commitString(text);
        return {true, describe(ic)};
    }

    std::tuple<bool, std::string> showPreedit(fcitx::InputContext *ic, const std::string &text) {
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
        // Cursor at the end (byte offset into the UTF-8 text) so the caret
        // follows the growing transcript.
        preedit.setCursor(static_cast<int>(text.size()));
        // Clients that advertise preedit support render it inline; others
        // (plain X11 apps, some terminals) only see the Fcitx5 panel.
        if (ic->capabilityFlags().test(fcitx::CapabilityFlag::Preedit)) {
            ic->inputPanel().setClientPreedit(preedit);
        } else {
            ic->inputPanel().setPreedit(preedit);
        }
        refresh(ic);
        preeditContext_ = ic->watch();
        return {true, describe(ic)};
    }

public:
    // D-Bus ClearPreedit: idempotent, never fails.
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
    // Remove the preedit from whichever context still shows it (if that
    // context is already gone the reference is null and only the watch is
    // dropped). Both preedit slots are reset because setPreedit may have
    // used either, depending on the client's capability at the time.
    void clearPreedit() {
        if (auto *ic = preeditContext_.get()) {
            ic->inputPanel().setClientPreedit(fcitx::Text());
            ic->inputPanel().setPreedit(fcitx::Text());
            refresh(ic);
        }
        preeditContext_.unwatch();
    }

    // Push the input panel state to the client (inline preedit) and to the
    // UI addon (panel); neither updates on its own after a set*Preedit.
    static void refresh(fcitx::InputContext *ic) {
        ic->updatePreedit();
        ic->updateUserInterface(fcitx::UserInterfaceComponent::InputPanel);
    }

    // mostRecentInputContext may still name a context that lost focus (the
    // user switched to a window without one); only act on it while it
    // actually has focus, otherwise text would go to the wrong field.
    fcitx::InputContext *focusedInputContext() {
        auto *ic = instance_->mostRecentInputContext();
        return ic && ic->hasFocus() ? ic : nullptr;
    }

    static std::string contextId(const fcitx::InputContext *ic) {
        constexpr char hex[] = "0123456789abcdef";
        std::string id;
        id.reserve(ic->uuid().size() * 2);
        for (auto byte : ic->uuid()) {
            id.push_back(hex[byte >> 4]);
            id.push_back(hex[byte & 0xf]);
        }
        return id;
    }

    // "program (frontend)": the client-reported program name ("unknown"
    // when it reported none) and the Fcitx5 frontend addon it connected
    // through. Returned as the detail string of every successful call.
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
    // Method table: (C++ member, D-Bus name, in signature, out signature).
    // The "bs" out signature is the (ok, detail) tuple every method returns;
    // changing it breaks rust/src/fcitx5.rs.
    FCITX_OBJECT_VTABLE_METHOD(commitString, "CommitString", "s", "bs");
    FCITX_OBJECT_VTABLE_METHOD(setPreedit, "SetPreedit", "s", "bs");
    FCITX_OBJECT_VTABLE_METHOD(clearPreeditMethod, "ClearPreedit", "", "bs");
    FCITX_OBJECT_VTABLE_METHOD(status, "Status", "", "bs");
    FCITX_OBJECT_VTABLE_METHOD(commitToContext, "CommitToContext", "ss", "bs");
    FCITX_OBJECT_VTABLE_METHOD(preeditToContext, "PreeditToContext", "ss", "bs");
    FCITX_OBJECT_VTABLE_METHOD(focusedContext, "FocusedContext", "", "bs");
};

// Addon instance: resolves the dbus addon (declared as a dependency in
// voiceless-bridge.conf, so it is loaded first) and registers the object
// on Fcitx5's own session-bus connection; no extra bus name is claimed.
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

// Entry point Fcitx5's addon manager uses to instantiate the module.
class VoicelessBridgeFactory : public fcitx::AddonFactory {
public:
    fcitx::AddonInstance *create(fcitx::AddonManager *manager) override {
        return new VoicelessBridge(manager->instance());
    }
};

} // namespace

FCITX_ADDON_FACTORY(VoicelessBridgeFactory);
