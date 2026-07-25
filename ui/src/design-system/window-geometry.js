/**
 * Projects a captured window frame through one XP-style resize grip.
 *
 * @param {"n"|"s"|"e"|"w"|"ne"|"nw"|"se"|"sw"} edge - Grip that owns the drag.
 * @param {{startX:number,startY:number,x:number,y:number,width:number,height:number}} origin - Pointer and frame captured on press.
 * @param {{x:number,y:number}} point - Current desktop pointer position.
 * @returns {{x:number,y:number,width:number,height:number,anchorRight:boolean,anchorBottom:boolean,right:number,bottom:number}} Candidate frame with its fixed far-edge metadata.
 */
export function projectResize(edge, origin, point) {
  const dx = point.x - origin.startX;
  const dy = point.y - origin.startY;
  let { x, y, width, height } = origin;
  if (edge.includes("e")) width = origin.width + dx;
  if (edge.includes("s")) height = origin.height + dy;
  if (edge.includes("w")) { x = origin.x + dx; width = origin.width - dx; }
  if (edge.includes("n")) { y = origin.y + dy; height = origin.height - dy; }
  return {
    x,
    y,
    width,
    height,
    anchorRight: edge.includes("w"),
    anchorBottom: edge.includes("n"),
    right: origin.x + origin.width,
    bottom: origin.y + origin.height,
  };
}

/**
 * Constrains a resize candidate to the desktop work area while keeping the
 * opposite edge fixed for north/west drags, matching native XP resizing.
 *
 * @param {{x:number,y:number,width:number,height:number,anchorRight:boolean,anchorBottom:boolean,right:number,bottom:number}} rect - Candidate resize frame.
 * @param {{x:number,y:number,width:number,height:number}} workArea - Taskbar-free desktop bounds.
 * @param {number} minWidth - Smallest allowed frame width.
 * @param {number} minHeight - Smallest allowed frame height.
 * @returns {{x:number,y:number,width:number,height:number}} Constrained window frame.
 */
export function constrainResize(rect, workArea, minWidth, minHeight) {
  let { x, y, width, height, anchorRight, anchorBottom, right, bottom } = rect;
  if (width < minWidth) { width = minWidth; if (anchorRight) x = right - minWidth; }
  if (height < minHeight) { height = minHeight; if (anchorBottom) y = bottom - minHeight; }

  // A north/west drag that crosses the work-area origin must shorten the frame
  // back to its fixed opposite edge. Clamping only x/y would move that edge,
  // making the window jump and grow unlike the native XP interaction.
  if (x < workArea.x) {
    if (anchorRight) width = right - workArea.x;
    x = workArea.x;
  }
  if (y < workArea.y) {
    if (anchorBottom) height = bottom - workArea.y;
    y = workArea.y;
  }

  const workRight = workArea.x + workArea.width;
  const workBottom = workArea.y + workArea.height;
  width = Math.max(minWidth, Math.min(width, workRight - x));
  height = Math.max(minHeight, Math.min(height, workBottom - y));
  return { x, y, width, height };
}
