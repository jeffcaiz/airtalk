//! All Slint components for airtalk's UI windows.
//!
//! Slint requires every Window to be created on the same OS thread
//! (per-thread platform state), and `slint::slint!` macro invocations
//! don't share components across blocks — each macro expands
//! independently. So every window + every shared theme component
//! (Card, PillButton, SectionTitle, …) lives in the single
//! `slint::slint!` block below. Rust imports the generated types by
//! name (`SettingsWindow`, eventually `RecoveryWindow`).
//!
//! Dark theme palette (keep changes centralized here, not duplicated
//! per window):
//!   * Background #0a1020
//!   * Card       #111a2e  (border #1f2a44)
//!   * Text fg    #f8fafc / #e2e8f0 / #94a3b8
//!   * Primary    #2c60eb → hover #3b82f6 → press #1d4ed8
//!
//! Colors are inlined in the components rather than pulled through
//! `global` singletons because `slint::slint!` is compiled per-macro
//! and global state across top-level components adds noise for a
//! two-window app.

#![cfg(windows)]

slint::slint! {
import { CheckBox, ComboBox, LineEdit, ScrollView, TextEdit } from "std-widgets.slint";

component SectionTitle inherits Text {
    color: #f8fafc;
    font-size: 15px;
    font-weight: 700;
}

component SectionHint inherits Text {
    color: #94a3b8;
    font-size: 12px;
    wrap: word-wrap;
}

component FieldLabel inherits Text {
    color: #e2e8f0;
    font-size: 12px;
    font-weight: 600;
}

component FieldHint inherits Text {
    color: #94a3b8;
    font-size: 11px;
    wrap: word-wrap;
}

component Card inherits Rectangle {
    background: #111a2e;
    border-radius: 12px;
    border-color: #1f2a44;
    border-width: 1px;
}

component Divider inherits Rectangle {
    height: 1px;
    background: #1f2a44;
}

component PillButton inherits Rectangle {
    in property <string> text;
    in property <bool> primary: false;
    in property <bool> enabled: true;
    callback clicked;

    height: 32px;
    horizontal-stretch: 0;
    border-radius: 8px;
    border-width: root.primary ? 0 : 1px;
    border-color: #2a3753;
    background: root.primary
        ? (ta.pressed ? #1d4ed8 : (ta.has-hover && root.enabled ? #3b82f6 : #2c60eb))
        : (ta.pressed ? #111a2e : (ta.has-hover && root.enabled ? #1a243c : #131c32));
    opacity: root.enabled ? 1 : 0.45;

    HorizontalLayout {
        padding-left: 16px;
        padding-right: 16px;
        alignment: center;
        Text {
            vertical-alignment: center;
            text: root.text;
            color: root.primary ? white : #e2e8f0;
            font-size: 13px;
            font-weight: root.primary ? 600 : 500;
        }
    }

    ta := TouchArea {
        enabled: root.enabled;
        clicked => { root.clicked(); }
    }
}

// Small colored chip that shows whether a secret (API key) is stored.
// Three states: Saved (green), Will-clear-on-save (amber), Not set (muted).
component StatusBadge inherits Rectangle {
    in property <bool> saved;
    in property <bool> pending-clear;

    property <string> badge-text: (root.saved && root.pending-clear)
        ? "Will clear on save"
        : (root.saved ? "Saved" : "Not set");
    property <color> badge-fg: (root.saved && root.pending-clear)
        ? #fde68a
        : (root.saved ? #bbf7d0 : #94a3b8);
    property <color> badge-bg: (root.saved && root.pending-clear)
        ? #b45309
        : (root.saved ? #14532d : #1f2a44);

    height: 22px;
    horizontal-stretch: 0;
    border-radius: 11px;
    background: badge-bg;

    HorizontalLayout {
        padding-left: 10px;
        padding-right: 10px;
        alignment: center;
        Text {
            vertical-alignment: center;
            text: root.badge-text;
            color: root.badge-fg;
            font-size: 11px;
            font-weight: 600;
        }
    }
}

export component SettingsWindow inherits Window {
    in-out property <bool> autostart-enabled;
    in-out property <string> asr-lang;
    in-out property <string> asr-base-url;
    in-out property <string> asr-key-input;
    in-out property <string> asr-hotwords;
    in-out property <bool> llm-enabled;
    in-out property <string> llm-key-input;
    in-out property <string> llm-base-url;
    in-out property <string> llm-model;
    in-out property <[string]> device-model;
    in-out property <int> device-index;
    // Region ComboBox picks a DashScope endpoint preset (Mainland /
    // International). Selecting a value overwrites `asr-base-url` and
    // `llm-base-url` in one shot. Derived on load from the URLs, not
    // persisted as its own field — URLs remain the source of truth.
    in-out property <int> region-index;
    // Credential Manager doesn't echo secrets back, so the UI can't show
    // the saved value. Instead we surface a visible badge driven by
    // these flags: `*-key-saved` comes from the snapshot at open time;
    // `*-key-pending-clear` is toggled by the Clear button and consumed
    // by the save flow.
    in-out property <bool> asr-key-saved;
    in-out property <bool> asr-key-pending-clear;
    in-out property <bool> llm-key-saved;
    in-out property <bool> llm-key-pending-clear;
    in-out property <string> status-text;

    callback save-requested();
    callback cancel-requested();

    width: 720px;
    height: 680px;
    title: "AirTalk";
    background: #0a1020;

    VerticalLayout {
        padding: 20px;
        spacing: 16px;

        // Header
        VerticalLayout {
            spacing: 4px;
            Text {
                text: "Settings";
                color: #f8fafc;
                font-size: 22px;
                font-weight: 700;
            }
            Text {
                text: "Microphone, speech recognition, and cleanup. Changes take effect on Save.";
                color: #94a3b8;
                font-size: 12px;
                wrap: word-wrap;
            }
        }

        // Scrollable body
        ScrollView {
            vertical-stretch: 1;
            horizontal-stretch: 1;

            VerticalLayout {
                padding-right: 4px;
                spacing: 14px;

                // ─── Startup ─────────────────────────────────────────
                Card {
                    VerticalLayout {
                        padding: 16px;
                        spacing: 10px;

                        HorizontalLayout {
                            alignment: space-between;
                            VerticalLayout {
                                spacing: 2px;
                                SectionTitle { text: "Startup"; }
                                SectionHint {
                                    text: "Start AirTalk automatically when you sign in to Windows.";
                                }
                            }
                            CheckBox {
                                text: "Launch at Startup";
                                checked <=> root.autostart-enabled;
                            }
                        }
                    }
                }

                // ─── Audio ───────────────────────────────────────────
                Card {
                    VerticalLayout {
                        padding: 16px;
                        spacing: 10px;

                        SectionTitle { text: "Audio"; }
                        SectionHint {
                            text: "Microphone used for capture. The tray icon switches the same setting.";
                        }

                        VerticalLayout {
                            spacing: 4px;
                            FieldLabel { text: "Input device"; }
                            ComboBox {
                                model: root.device-model;
                                current-index <=> root.device-index;
                            }
                        }
                    }
                }

                // ─── Speech recognition ──────────────────────────────
                Card {
                    VerticalLayout {
                        padding: 16px;
                        spacing: 12px;

                        SectionTitle { text: "Speech recognition"; }
                        SectionHint {
                            text: "Qwen3-ASR on DashScope. API key is stored in Windows Credential Manager.";
                        }

                        // ── Region preset ────────────────────────────
                        // Selecting a region overwrites both the ASR
                        // and LLM base URL fields in one shot. Users
                        // outside China should pick International so
                        // their intl-region API key authenticates
                        // against the right endpoint. The two URLs
                        // below remain editable for other providers.
                        VerticalLayout {
                            spacing: 4px;
                            FieldLabel { text: "Region"; }
                            FieldHint {
                                text: "DashScope endpoint preset. Overwrites both base URLs below.";
                            }
                            ComboBox {
                                model: ["Mainland China", "International"];
                                current-index <=> root.region-index;
                                // Slint std-widgets ComboBox.selected delivers
                                // the string value, NOT the index. Comparing
                                // `value == 0` silently evaluates to false
                                // regardless of selection — always hit the
                                // `else` branch. Match the string instead.
                                // KEEP URLS IN SYNC with MAINLAND_ASR_URL /
                                // MAINLAND_LLM_URL in settings.rs.
                                selected(value) => {
                                    if (value == "Mainland China") {
                                        root.asr-base-url = "https://dashscope.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation";
                                        root.llm-base-url = "https://dashscope.aliyuncs.com/compatible-mode/v1";
                                    } else {
                                        root.asr-base-url = "https://dashscope-intl.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation";
                                        root.llm-base-url = "https://dashscope-intl.aliyuncs.com/compatible-mode/v1";
                                    }
                                }
                            }
                        }

                        VerticalLayout {
                            spacing: 4px;
                            HorizontalLayout {
                                spacing: 10px;
                                alignment: start;
                                FieldLabel {
                                    text: "DashScope API key";
                                    vertical-alignment: center;
                                }
                                StatusBadge {
                                    saved: root.asr-key-saved;
                                    pending-clear: root.asr-key-pending-clear;
                                }
                            }
                            HorizontalLayout {
                                spacing: 8px;
                                LineEdit {
                                    horizontal-stretch: 1;
                                    placeholder-text: "sk-…";
                                    // When a key is safely stored, show a masked
                                    // placeholder-shaped string and lock the field
                                    // so the user can't accidentally type over it.
                                    // The field unlocks (and empties) the moment
                                    // Clear is clicked.
                                    text: (root.asr-key-saved && !root.asr-key-pending-clear)
                                        ? "sk-******"
                                        : root.asr-key-input;
                                    enabled: !root.asr-key-saved || root.asr-key-pending-clear;
                                    edited(value) => {
                                        root.asr-key-input = value;
                                    }
                                }
                                PillButton {
                                    // Toggleable: "Clear" arms the delete, "Undo"
                                    // cancels it. Only available when there's a
                                    // saved key to act on.
                                    text: root.asr-key-pending-clear ? "Undo" : "Clear";
                                    enabled: root.asr-key-saved;
                                    clicked => {
                                        root.asr-key-pending-clear = !root.asr-key-pending-clear;
                                    }
                                }
                            }
                        }

                        HorizontalLayout {
                            spacing: 12px;
                            VerticalLayout {
                                spacing: 4px;
                                horizontal-stretch: 1;
                                FieldLabel { text: "Language"; }
                                FieldHint { text: "Code like zh or en, or auto to detect."; }
                                LineEdit {
                                    text <=> root.asr-lang;
                                }
                            }
                        }

                        VerticalLayout {
                            spacing: 4px;
                            FieldLabel { text: "Base URL"; }
                            FieldHint {
                                text: "Qwen3-ASR endpoint. Use dashscope-intl.aliyuncs.com if your DashScope account is in the International region.";
                            }
                            LineEdit {
                                text <=> root.asr-base-url;
                            }
                        }

                        VerticalLayout {
                            spacing: 4px;
                            FieldLabel { text: "Hotwords"; }
                            FieldHint {
                                text: "One term per line. Letters, digits, spaces, and - _ . + # only. Examples: React, Vite, TypeScript, Node.js, C++, 接口.";
                            }
                            TextEdit {
                                text <=> root.asr-hotwords;
                                height: 120px;
                            }
                        }
                    }
                }

                // ─── Cleanup (LLM) ───────────────────────────────────
                Card {
                    VerticalLayout {
                        padding: 16px;
                        spacing: 12px;

                        HorizontalLayout {
                            alignment: space-between;
                            VerticalLayout {
                                spacing: 2px;
                                SectionTitle { text: "Cleanup"; }
                                SectionHint {
                                    text: "Optional LLM pass to tidy punctuation and remove filler words.";
                                }
                            }
                            CheckBox {
                                text: "Enabled";
                                checked <=> root.llm-enabled;
                            }
                        }

                        Divider {}

                        VerticalLayout {
                            spacing: 4px;
                            HorizontalLayout {
                                spacing: 10px;
                                alignment: start;
                                FieldLabel {
                                    text: "API key";
                                    vertical-alignment: center;
                                }
                                StatusBadge {
                                    saved: root.llm-key-saved;
                                    pending-clear: root.llm-key-pending-clear;
                                }
                            }
                            HorizontalLayout {
                                spacing: 8px;
                                LineEdit {
                                    horizontal-stretch: 1;
                                    placeholder-text: "sk-…";
                                    text: (root.llm-key-saved && !root.llm-key-pending-clear)
                                        ? "sk-******"
                                        : root.llm-key-input;
                                    // Two gates: the whole cleanup section off →
                                    // field disabled; a saved key present and not
                                    // pending-clear → also disabled (protected).
                                    enabled: root.llm-enabled
                                        && (!root.llm-key-saved || root.llm-key-pending-clear);
                                    edited(value) => {
                                        root.llm-key-input = value;
                                    }
                                }
                                PillButton {
                                    text: root.llm-key-pending-clear ? "Undo" : "Clear";
                                    enabled: root.llm-enabled && root.llm-key-saved;
                                    clicked => {
                                        root.llm-key-pending-clear = !root.llm-key-pending-clear;
                                    }
                                }
                            }
                        }

                        HorizontalLayout {
                            spacing: 12px;
                            VerticalLayout {
                                spacing: 4px;
                                horizontal-stretch: 2;
                                FieldLabel { text: "Base URL"; }
                                LineEdit {
                                    text <=> root.llm-base-url;
                                    enabled: root.llm-enabled;
                                }
                            }
                            VerticalLayout {
                                spacing: 4px;
                                horizontal-stretch: 1;
                                FieldLabel { text: "Model"; }
                                LineEdit {
                                    text <=> root.llm-model;
                                    enabled: root.llm-enabled;
                                }
                            }
                        }
                    }
                }
            }
        }

        // Footer
        VerticalLayout {
            spacing: 10px;

            if root.status-text != "": Rectangle {
                background: #2a0f1a;
                border-radius: 8px;
                border-color: #7f1d1d;
                border-width: 1px;
                HorizontalLayout {
                    padding: 10px;
                    Text {
                        text: root.status-text;
                        color: #fecaca;
                        font-size: 12px;
                        wrap: word-wrap;
                    }
                }
            }

            HorizontalLayout {
                alignment: end;
                spacing: 8px;
                PillButton {
                    text: "Cancel";
                    clicked => { root.cancel-requested(); }
                }
                PillButton {
                    text: "Save";
                    primary: true;
                    clicked => { root.save-requested(); }
                }
            }
        }
    }
}

// ─── Recovery popup ────────────────────────────────────────────────────
// Shown when paste into the focused app fails. Displays the transcript
// with a "复制" button so the user can get the text to their clipboard
// manually. Expected to be topmost and non-focus-stealing; the Rust
// side applies WS_EX_NOACTIVATE + SWP_NOACTIVATE via the raw HWND
// after show(), because Slint doesn't expose those extended styles.

component CloseGlyph inherits Rectangle {
    callback clicked;
    width: 22px; height: 22px;
    border-radius: 11px;
    background: close-ta.has-hover ? #1f2a44 : transparent;
    Text {
        // U+00D7 (Latin-1 multiplication sign) — available in every
        // system font. U+2715 ("✕") isn't, and renders as tofu on
        // systems whose default Slint font lacks dingbats.
        text: "×";
        color: #94a3b8;
        font-size: 16px;
        horizontal-alignment: center;
        vertical-alignment: center;
    }
    close-ta := TouchArea {
        clicked => { root.clicked(); }
    }
}

export component RecoveryWindow inherits Window {
    in property <string> body-text;
    callback copy-requested();
    callback dismiss-requested();
    // Window drag is handled by the Rust side subclassing WM_NCHITTEST
    // to return HTCAPTION for the header region — Windows then runs its
    // native modal drag loop without Slint ever seeing the pointer-down
    // event. That keeps Slint's TouchArea state clean (no stuck cursor
    // after drag) and gives us real OS snap-to-edge behavior for free.

    width: 440px;
    height: 280px;
    no-frame: true;
    always-on-top: true;
    title: "AirTalk — paste failed";
    background: #0a1020;
    default-font-size: 13px;
    // Route keyboard input to the FocusScope below so Esc works as
    // soon as the popup appears, without requiring the user to click
    // into the window first.
    forward-focus: fs;

    VerticalLayout {
        padding: 16px;
        spacing: 10px;

        // Header: title text (drag region, handled by Rust) + close glyph.
        // The header's vertical extent (~52 logical px) is the target
        // the WM_NCHITTEST subclass checks against — keep the layout
        // roughly in sync if you change title font sizes.
        HorizontalLayout {
            alignment: space-between;
            VerticalLayout {
                horizontal-stretch: 1;
                spacing: 2px;
                Text {
                    text: "Paste failed";
                    color: #f8fafc;
                    font-size: 14px;
                    font-weight: 700;
                }
                Text {
                    text: "Copy the transcript manually, then paste it where you need it.";
                    color: #94a3b8;
                    font-size: 11px;
                    wrap: word-wrap;
                }
            }
            CloseGlyph {
                clicked => { root.dismiss-requested(); }
            }
        }

        // Body (scrolls if long)
        Rectangle {
            vertical-stretch: 1;
            background: #111a2e;
            border-radius: 8px;
            border-color: #1f2a44;
            border-width: 1px;
            ScrollView {
                VerticalLayout {
                    padding: 10px;
                    Text {
                        text: root.body-text;
                        color: #e2e8f0;
                        font-size: 13px;
                        wrap: word-wrap;
                    }
                }
            }
        }

        // Footer: Copy button (right-aligned)
        HorizontalLayout {
            alignment: end;
            spacing: 8px;
            PillButton {
                text: "Dismiss";
                clicked => { root.dismiss-requested(); }
            }
            PillButton {
                text: "复制";
                primary: true;
                clicked => { root.copy-requested(); }
            }
        }
    }

    // Esc closes the popup.
    fs := FocusScope {
        key-pressed(event) => {
            if (event.text == Key.Escape) {
                root.dismiss-requested();
                accept
            } else {
                reject
            }
        }
    }
}
}
