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

        function activateSub(panel) {
            var subs = panel.querySelectorAll(":scope > .subtabs > .subtab");
            var subpanels = panel.querySelectorAll(":scope > .subpanel");
            Array.prototype.forEach.call(subs, function (t, i) {
                t.classList.toggle("active", i === 0);
            });
            Array.prototype.forEach.call(subpanels, function (p, i) {
                p.classList.toggle("active", i === 0);
            });
        }

        function activate(name) {
            tabs.forEach(function (t) {
                t.classList.toggle("active", t.getAttribute("data-tab") === name);
            });
            panels.forEach(function (p) {
                var on = p.getAttribute("data-panel") === name;
                p.classList.toggle("active", on);
                if (on) activateSub(p);
            });
        }

        tabs.forEach(function (t) {
            t.addEventListener("click", function () {
                activate(t.getAttribute("data-tab"));
            });
        });

        // Chain subtab clicks (delegated: they live inside panels)
        root.addEventListener("click", function (e) {
            var sub = e.target.closest ? e.target.closest(".subtab") : null;
            if (!sub || !root.contains(sub)) return;
            var panel = sub.closest(".tab-panel");
            Array.prototype.forEach.call(
                panel.querySelectorAll(":scope > .subtabs > .subtab"),
                function (t) { t.classList.toggle("active", t === sub); }
            );
            var name = sub.getAttribute("data-chain");
            Array.prototype.forEach.call(
                panel.querySelectorAll(":scope > .subpanel"),
                function (p) { p.classList.toggle("active", p.getAttribute("data-chain") === name); }
            );
        });

        root.classList.add("tabs-on");
        activate(tabs[0].getAttribute("data-tab"));
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
