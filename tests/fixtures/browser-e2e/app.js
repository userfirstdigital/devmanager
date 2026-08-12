document.querySelector('[data-testid="fixture-form"]').addEventListener("submit", (event) => {
  event.preventDefault();
  document.querySelector('[data-testid="form-result"]').textContent = "Form submitted";
});
document.querySelector('[data-testid="fixture-upload"]').addEventListener("change", (event) => {
  document.querySelector('[data-testid="upload-result"]').textContent =
    `${event.target.files.length} file(s) selected`;
});
document.querySelector('[data-testid="permission-target"]').addEventListener("click", () => {
  document.querySelector('[data-testid="permission-result"]').textContent =
    "Permission marker acknowledged";
});
