// indexrs keyboard shortcuts and UI interactions
(function() {
    "use strict";

    var selectedIndex = -1;

    function getFileResults() {
        return document.querySelectorAll(".file-result");
    }

    function clearSelection() {
        var results = getFileResults();
        results.forEach(function(el) { el.classList.remove("selected"); });
        selectedIndex = -1;
    }

    function selectResult(index) {
        var results = getFileResults();
        if (results.length === 0) return;
        clearSelection();
        selectedIndex = Math.max(0, Math.min(index, results.length - 1));
        var el = results[selectedIndex];
        el.classList.add("selected");
        el.scrollIntoView({ block: "nearest", behavior: "smooth" });
    }

    function openSelected() {
        var results = getFileResults();
        if (selectedIndex < 0 || selectedIndex >= results.length) return;
        var link = results[selectedIndex].querySelector(".file-header a, .file-header .path");
        if (link && link.href) {
            window.location.href = link.href;
        } else if (link && link.closest("a")) {
            window.location.href = link.closest("a").href;
        }
    }

    function focusSearch() {
        var input = document.querySelector(".search-input");
        if (input) {
            input.focus();
            input.select();
        }
    }

    function isInputFocused() {
        var el = document.activeElement;
        return el && (el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.tagName === "SELECT");
    }

    function toggleHelp() {
        var overlay = document.querySelector(".help-overlay");
        if (overlay) {
            overlay.classList.toggle("visible");
        }
    }

    function navigatePage(direction) {
        var links = document.querySelectorAll(".pagination button");
        for (var i = 0; i < links.length; i++) {
            var text = links[i].textContent.trim().toLowerCase();
            if (direction === "next" && (text === "next" || text.indexOf("next") !== -1)) {
                links[i].click();
                return;
            }
            if (direction === "prev" && (text === "prev" || text.indexOf("prev") !== -1)) {
                links[i].click();
                return;
            }
        }
    }

    // Quick-open modal (Go to Symbol)
    var quickopenSelectedIndex = -1;

    function openQuickOpen() {
        var overlay = document.getElementById("quickopen-overlay");
        if (!overlay) return;
        overlay.classList.add("visible");
        var input = document.getElementById("quickopen-input");
        if (input) {
            input.value = "";
            input.focus();
        }
        quickopenSelectedIndex = -1;
    }

    function closeQuickOpen() {
        var overlay = document.getElementById("quickopen-overlay");
        if (overlay) overlay.classList.remove("visible");
        quickopenSelectedIndex = -1;
    }

    function getQuickOpenResults() {
        return document.querySelectorAll("#quickopen-results .symbol-result");
    }

    function selectQuickOpenResult(index) {
        var results = getQuickOpenResults();
        if (results.length === 0) return;
        results.forEach(function(el) { el.classList.remove("selected"); });
        quickopenSelectedIndex = Math.max(0, Math.min(index, results.length - 1));
        var el = results[quickopenSelectedIndex];
        el.classList.add("selected");
        el.scrollIntoView({ block: "nearest", behavior: "smooth" });
    }

    function openQuickOpenSelected() {
        var results = getQuickOpenResults();
        if (quickopenSelectedIndex < 0 || quickopenSelectedIndex >= results.length) return;
        var link = results[quickopenSelectedIndex].querySelector("a");
        if (link && link.href) {
            window.location.href = link.href;
        }
    }

    document.addEventListener("keydown", function(e) {
        // Quick-open keyboard handling
        var quickopen = document.getElementById("quickopen-overlay");
        if (quickopen && quickopen.classList.contains("visible")) {
            if (e.key === "Escape") {
                e.preventDefault();
                closeQuickOpen();
                return;
            }
            if (e.key === "ArrowDown" || (e.key === "j" && e.ctrlKey)) {
                e.preventDefault();
                selectQuickOpenResult(quickopenSelectedIndex + 1);
                return;
            }
            if (e.key === "ArrowUp" || (e.key === "k" && e.ctrlKey)) {
                e.preventDefault();
                selectQuickOpenResult(quickopenSelectedIndex - 1);
                return;
            }
            if (e.key === "Enter") {
                e.preventDefault();
                openQuickOpenSelected();
                return;
            }
            return; // Let all other keys pass to the quickopen input
        }

        // Close help overlay on any key if visible
        var overlay = document.querySelector(".help-overlay");
        if (overlay && overlay.classList.contains("visible") && e.key !== "?") {
            overlay.classList.remove("visible");
            e.preventDefault();
            return;
        }

        // Don't capture if typing in an input (except Escape)
        if (isInputFocused() && e.key !== "Escape") {
            return;
        }

        switch (e.key) {
            case "/":
                e.preventDefault();
                focusSearch();
                break;
            case "Escape":
                if (isInputFocused()) {
                    document.activeElement.blur();
                } else {
                    clearSelection();
                }
                break;
            case "j":
                e.preventDefault();
                selectResult(selectedIndex + 1);
                break;
            case "k":
                e.preventDefault();
                selectResult(selectedIndex - 1);
                break;
            case "Enter":
                if (!isInputFocused()) {
                    e.preventDefault();
                    openSelected();
                }
                break;
            case "n":
                e.preventDefault();
                navigatePage("next");
                break;
            case "p":
                e.preventDefault();
                navigatePage("prev");
                break;
            case "q":
            case "Backspace":
                // Back to results from file preview
                if (document.querySelector(".file-preview-header")) {
                    e.preventDefault();
                    window.history.back();
                }
                break;
            case "?":
                e.preventDefault();
                toggleHelp();
                break;
            case "@":
                e.preventDefault();
                openQuickOpen();
                break;
        }
    });

    // Reset selection when htmx swaps in new content
    document.addEventListener("htmx:afterSwap", function(e) {
        clearSelection();
        if (e.target && e.target.id === "quickopen-results") {
            quickopenSelectedIndex = -1;
        }
    });

    // Theme toggle (light <-> dark)
    var themeToggle = document.getElementById("theme-toggle");
    if (themeToggle) {
        themeToggle.addEventListener("click", function() {
            var next = document.documentElement.getAttribute("data-theme") === "dark" ? "light" : "dark";
            document.documentElement.setAttribute("data-theme", next);
            localStorage.setItem("theme", next);
        });
    }

    // Toggle collapsible sections (e.g. segment detail table)
    // Persist expanded state in localStorage under "expanded-sections" (JSON array of IDs).
    var EXPANDED_KEY = "expanded-sections";
    function getExpandedSections() {
        try { return JSON.parse(localStorage.getItem(EXPANDED_KEY)) || []; }
        catch (_) { return []; }
    }
    function saveExpandedSections(ids) {
        localStorage.setItem(EXPANDED_KEY, JSON.stringify(ids));
    }

    // Restore previously expanded sections on page load.
    getExpandedSections().forEach(function(id) {
        var target = document.getElementById(id);
        var toggle = document.querySelector('[data-toggle="' + id + '"]');
        if (target) {
            target.style.display = "";
            if (toggle) toggle.classList.add("is-expanded");
        }
    });

    document.addEventListener("click", function(e) {
        var toggle = e.target.closest("[data-toggle]");
        if (!toggle) return;
        var id = toggle.getAttribute("data-toggle");
        var target = document.getElementById(id);
        if (target) {
            var isHidden = target.style.display === "none";
            target.style.display = isHidden ? "" : "none";
            toggle.classList.toggle("is-expanded", isHidden);

            var expanded = getExpandedSections();
            if (isHidden) {
                if (expanded.indexOf(id) === -1) expanded.push(id);
            } else {
                expanded = expanded.filter(function(x) { return x !== id; });
            }
            saveExpandedSections(expanded);
        }
    });

    // Close quick-open on backdrop click
    document.addEventListener("click", function(e) {
        if (e.target.id === "quickopen-overlay") {
            closeQuickOpen();
        }
    });

    // Search mode toggle (text vs symbols)
    function setSearchMode(mode) {
        var textBtn = document.getElementById("mode-text");
        var symBtn = document.getElementById("mode-symbol");
        var input = document.querySelector(".search-input");
        var modeInput = document.getElementById("search-mode");
        if (!textBtn || !symBtn || !input || !modeInput) return;

        modeInput.value = mode;
        textBtn.classList.toggle("mode-btn--active", mode === "text");
        symBtn.classList.toggle("mode-btn--active", mode === "symbol");

        input.setAttribute("placeholder", mode === "symbol"
            ? "Search symbols... (functions, structs, classes)"
            : "Search code... (press / to focus, ? for help)");

        // Re-trigger search with current value
        if (input.value) {
            htmx.trigger(input, "search");
        }
    }

    document.addEventListener("mousedown", function(e) {
        var btn = e.target.closest(".mode-btn");
        if (!btn || !btn.dataset.mode) return;
        e.preventDefault(); // prevent focus steal from search input
        setSearchMode(btn.dataset.mode);
    });

    // Sidebar repo selection
    document.addEventListener("click", function(e) {
        var repo = e.target.closest(".sidebar-repo");
        if (!repo) return;
        // Update active state
        document.querySelectorAll(".sidebar-repo").forEach(function(el) {
            el.classList.remove("sidebar-repo--active");
        });
        repo.classList.add("sidebar-repo--active");
        // Check the radio so htmx includes it
        var radio = repo.querySelector(".sidebar-repo-radio");
        if (radio) radio.checked = true;
    });

    // Outline panel toggle
    var outlineToggle = document.getElementById("outline-toggle");
    if (outlineToggle) {
        outlineToggle.addEventListener("click", function() {
            var panel = document.getElementById("outline-panel");
            if (panel) panel.classList.toggle("hidden");
        });
    }

    // Outline click-to-scroll
    document.addEventListener("click", function(e) {
        var item = e.target.closest(".outline-item");
        if (!item) return;
        e.preventDefault();
        var line = item.getAttribute("data-line");
        var target = document.getElementById("L" + line);
        if (target) {
            target.scrollIntoView({ block: "center", behavior: "smooth" });
            // Flash highlight
            target.classList.add("code-line--highlight");
            setTimeout(function() { target.classList.remove("code-line--highlight"); }, 1500);
        }
    });
})();
