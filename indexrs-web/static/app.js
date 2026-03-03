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
        var links = document.querySelectorAll(".pagination a");
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

    document.addEventListener("keydown", function(e) {
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
        }
    });

    // Reset selection when htmx swaps in new content
    document.addEventListener("htmx:afterSwap", function() {
        clearSelection();
    });

    // Update status badge when repo changes
    var repoSelect = document.getElementById("repo-select");
    if (repoSelect) {
        repoSelect.addEventListener("change", function() {
            var badge = document.getElementById("repo-status");
            if (!badge) return;
            fetch("/repo-status?repo-select=" + encodeURIComponent(repoSelect.value))
                .then(function(r) { return r.text(); })
                .then(function(text) { badge.textContent = text; });
        });
    }

    // Auto-focus search on page load
    document.addEventListener("DOMContentLoaded", function() {
        focusSearch();
    });
})();
