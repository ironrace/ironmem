const copyButtons = document.querySelectorAll("[data-copy-target]");

copyButtons.forEach((button) => {
  button.addEventListener("click", async () => {
    const targetId = button.getAttribute("data-copy-target");
    const target = targetId ? document.getElementById(targetId) : null;
    if (!target || !navigator.clipboard) {
      return;
    }

    const originalText = button.textContent;
    try {
      await navigator.clipboard.writeText(target.textContent.trim());
      button.textContent = "Copied";
      window.setTimeout(() => {
        button.textContent = originalText;
      }, 1600);
    } catch (_error) {
      button.textContent = "Select";
      window.setTimeout(() => {
        button.textContent = originalText;
      }, 1600);
    }
  });
});
