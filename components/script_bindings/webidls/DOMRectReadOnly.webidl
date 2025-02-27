/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://drafts.fxtf.org/geometry/#domrect

[Exposed=(Window,Worker),
 Serializable]
interface DOMRectReadOnly {
  [Throws] constructor(optional unrestricted double x = 0, optional unrestricted double y = 0,
              optional unrestricted double width = 0, optional unrestricted double height = 0);

  [NewObject] static DOMRectReadOnly fromRect(optional DOMRectInit other = {});

  readonly attribute unrestricted double x;
  readonly attribute unrestricted double y;
  readonly attribute unrestricted double width;
  readonly attribute unrestricted double height;
  readonly attribute unrestricted double top;
  readonly attribute unrestricted double right;
  readonly attribute unrestricted double bottom;
  readonly attribute unrestricted double left;

  [Default] object toJSON();
};

// https://drafts.fxtf.org/geometry/#dictdef-domrectinit
dictionary DOMRectInit {
  unrestricted double x = 0;
  unrestricted double y = 0;
  unrestricted double width = 0;
  unrestricted double height = 0;
};
