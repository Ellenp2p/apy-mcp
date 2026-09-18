/* APY MCP web UI — minimal client script.
 * The page itself is Dioxus SSR; JS is only needed to capture the OAuth
 * redirect token (/?oauth=success&user=...&token=...) for later API/MCP use. */
(function () {
    "use strict";
    var params = new URLSearchParams(location.search);
    var token = params.get("token");
    if (params.get("oauth") === "success" && token) {
        localStorage.setItem("apy_mcp_token", token);
        localStorage.setItem("apy_mcp_user", params.get("user") || "");
        // Clean the token out of the URL
        history.replaceState(null, "", location.pathname);
        // Reflect login state in the header without a page reload
        var area = document.querySelector(".auth-area");
        if (area) {
            var user = localStorage.getItem("apy_mcp_user");
            area.innerHTML = '<span class="user">🐙 ' + (user || "已登录") + "</span>";
        }
    } else if (params.get("error") === "not_allowed") {
        var main = document.querySelector(".container");
        if (main) {
            var div = document.createElement("div");
            div.className = "notice error";
            div.textContent = "该 GitHub 账号不在允许列表中，访问被拒绝。";
            main.insertBefore(div, main.firstChild);
        }
        history.replaceState(null, "", location.pathname);
    }
    // Show stored login state on plain loads
    var stored = localStorage.getItem("apy_mcp_token");
    if (stored && !token) {
        var area2 = document.querySelector(".auth-area");
        if (area2) {
            var user2 = localStorage.getItem("apy_mcp_user");
            area2.innerHTML = '<span class="user">🐙 ' + (user2 || "已登录") + "</span>" +
                ' <button class="btn" onclick="localStorage.removeItem(\'apy_mcp_token\');localStorage.removeItem(\'apy_mcp_user\');location.reload()">退出</button>';
        }
    }
})();

/* Results tabs: protocol level (data-tab / data-panel) with chain subtabs
 * (data-chain) inside each panel. Without JS the `tabs-on` class is never
 * added and all panels stay visible, stacked.
 *
 * Toggle semantics: nothing selected by default (no `.has-selection` on
 * `.results`, all panels visible). Click a tab to filter down to that
 * protocol's pools; click the same tab again to clear the selection and
 * show everything again. Chain subtabs work the same way within their
 * parent protocol panel. Subtab clicks also activate their parent
 * protocol tab if it isn't already active.
 *
 * htmx swaps the `#results-region` fragment in place on every sort/filter
 * click, so we expose `initResultsTabs()` and re-run it after each swap via
 * the `htmx:afterSwap` event — otherwise the freshly injected `.results`
 * block has no `tabs-on` class (CSS hides the tab bar) and the new tab
 * buttons have no click handlers. */
(function () {
    "use strict";

    function initResultsTabs(root) {
        if (!root || root.dataset.tabsInit === "1") return;
        var tabs = Array.prototype.slice.call(root.querySelectorAll(":scope > .tabs > .tab"));
        if (!tabs.length) return;
        var panels = Array.prototype.slice.call(root.querySelectorAll(":scope > .tab-panel"));

        function clearAllActive() {
            Array.prototype.forEach.call(
                root.querySelectorAll(".tab.active, .subtab.active, .tab-panel.active, .subpanel.active"),
                function (el) { el.classList.remove("active"); }
            );
            root.classList.remove("has-selection");
        }

        function activateTab(tabName) {
            var tab = tabs.filter(function (t) { return t.getAttribute("data-tab") === tabName; })[0];
            var panel = panels.filter(function (p) { return p.getAttribute("data-panel") === tabName; })[0];
            if (!tab || !panel) return;
            var wasActive = tab.classList.contains("active");
            clearAllActive();
            if (!wasActive) {
                tab.classList.add("active");
                panel.classList.add("active");
                // No first-chain auto-select: the user can pick a chain or
                // see all chains for this protocol. CSS only hides non-active
                // subpanels when `.has-selection` is set, but subtab clicks
                // also add it, so this works either way.
                root.classList.add("has-selection");
            }
        }

        function activateSubtab(chain, panel) {
            var subs = Array.prototype.slice.call(panel.querySelectorAll(":scope > .subtabs > .subtab"));
            var subpanels = Array.prototype.slice.call(panel.querySelectorAll(":scope > .subpanel"));
            var sub = subs.filter(function (s) { return s.getAttribute("data-chain") === chain; })[0];
            var subpanel = subpanels.filter(function (p) { return p.getAttribute("data-chain") === chain; })[0];
            if (!sub || !subpanel) return;
            var wasActive = sub.classList.contains("active");

            // Ensure the parent protocol tab/panel is active (independent dim).
            var tabName = panel.getAttribute("data-panel");
            var parentTab = tabs.filter(function (t) { return t.getAttribute("data-tab") === tabName; })[0];
            if (parentTab && !parentTab.classList.contains("active")) {
                tabs.forEach(function (t) {
                    t.classList.toggle("active", t.getAttribute("data-tab") === tabName);
                });
                panels.forEach(function (p) {
                    p.classList.toggle("active", p === panel);
                });
            }
            // Clear subtabs/subpanels within this panel only.
            subs.forEach(function (s) { s.classList.remove("active"); });
            subpanels.forEach(function (p) { p.classList.remove("active"); });

            if (!wasActive) {
                sub.classList.add("active");
                subpanel.classList.add("active");
                root.classList.add("has-selection");
            } else {
                // Subtab deselected: if nothing else is active, drop has-selection.
                var anyActive = root.querySelector(".tab.active, .subtab.active");
                if (!anyActive) root.classList.remove("has-selection");
            }
        }

        tabs.forEach(function (t) {
            t.addEventListener("click", function () {
                activateTab(t.getAttribute("data-tab"));
            });
        });

        // Chain subtab clicks (delegated: they live inside panels)
        root.addEventListener("click", function (e) {
            var sub = e.target.closest ? e.target.closest(".subtab") : null;
            if (!sub || !root.contains(sub)) return;
            var panel = sub.closest(".tab-panel");
            if (!panel) return;
            e.stopPropagation();
            activateSubtab(sub.getAttribute("data-chain"), panel);
        });

        // Tabs are visible but no panel is active by default — `.has-selection`
        // is only added on first user click.
        root.classList.add("tabs-on");
        root.dataset.tabsInit = "1";
    }

    window.initResultsTabs = initResultsTabs;

    function initAll() {
        document.querySelectorAll(".results").forEach(initResultsTabs);
    }

    if (document.readyState === "loading") {
        document.addEventListener("DOMContentLoaded", initAll);
    } else {
        initAll();
    }

    // htmx re-injects the results region on sort/filter clicks — re-bind.
    document.body.addEventListener("htmx:afterSwap", function (e) {
        var tgt = e.target;
        if (tgt && tgt.querySelectorAll) {
            tgt.querySelectorAll(".results").forEach(initResultsTabs);
        }
    });
})();
