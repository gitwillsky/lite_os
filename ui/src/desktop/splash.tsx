import React from "react";

/**
 * Renders the approved aurora splash while the complete desktop is already
 * mounted underneath it. CSS owns the complete timeline; keeping the DOM node
 * mounted after its forwards-filled `display: none` exit avoids a JavaScript
 * lifecycle clock that could drift from the presentation timeline.
 *
 * @returns The full-viewport splash overlay.
 */
export function Splash() {
  return (
    <div className="splash">
      <img className="splash__background" src="assets/aurora-background.png"/>
      <div className="splash__identity">
        <div className="splash__logo-glow"/>
        <img className="splash__logo" src="assets/aurora-logo.png"/>
        <span className="splash__title">LiteOS</span>
      </div>
      <div className="splash__loading">
        <div className="splash__track">
          <div className="splash__highlight"/>
        </div>
        <span className="splash__status">Starting your workspace</span>
      </div>
    </div>
  );
}
