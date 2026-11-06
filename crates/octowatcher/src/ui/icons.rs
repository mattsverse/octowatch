//! Line icons on a 24-point grid, drawn with a 1.75 stroke in the text colour.

use rune::Icon;

macro_rules! line {
    ($($d:literal),+ $(,)?) => {
        concat!($("<path d=\"", $d, "\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"1.75\" stroke-linecap=\"round\" stroke-linejoin=\"round\"/>"),+)
    };
}

macro_rules! ring {
    ($cx:literal, $cy:literal, $r:literal) => {
        concat!(
            "<circle cx=\"",
            $cx,
            "\" cy=\"",
            $cy,
            "\" r=\"",
            $r,
            "\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"1.75\"/>"
        )
    };
}

macro_rules! icon {
    ($name:ident, $id:literal, $body:expr) => {
        pub static $name: Icon = Icon {
            name: concat!("ow-", $id),
            view: 24.0,
            body: $body,
        };
    };
}

icon!(
    REFRESH,
    "refresh",
    line!(
        "M20 11.5A8 8 0 0 0 6.3 6.3L4 8.5",
        "M4 4v4.5h4.5",
        "M4 12.5a8 8 0 0 0 13.7 5.2l2.3-2.2",
        "M20 20v-4.5h-4.5"
    )
);
icon!(
    FOLDER,
    "folder",
    line!(
        "M3.5 6.5A1.5 1.5 0 0 1 5 5h4l2 2.5h8A1.5 1.5 0 0 1 20.5 9v8.5A1.5 1.5 0 0 1 19 19H5a1.5 1.5 0 0 1-1.5-1.5Z"
    )
);
icon!(
    MOON,
    "moon",
    line!("M12.5 3.5a6.5 6.5 0 0 0 8 8 8.5 8.5 0 1 1-8-8Z")
);
icon!(
    SUN,
    "sun",
    concat!(
        ring!("12", "12", "4"),
        line!(
            "M12 2.5v2",
            "M12 19.5v2",
            "M4.6 4.6l1.4 1.4",
            "M18 18l1.4 1.4",
            "M2.5 12h2",
            "M19.5 12h2",
            "M4.6 19.4L6 18",
            "M18 6l1.4-1.4"
        )
    )
);
icon!(
    PULL_REQUEST,
    "pull-request",
    concat!(
        ring!("6", "6", "2.5"),
        ring!("6", "18", "2.5"),
        ring!("18", "18", "2.5"),
        line!(
            "M6 8.5v7",
            "M18 15.5V9a2.5 2.5 0 0 0-2.5-2.5H11",
            "M13.5 4 11 6.5 13.5 9"
        )
    )
);
icon!(
    FILTER,
    "filter",
    line!("M4 6.5h16", "M7 12h10", "M10 17.5h4")
);
icon!(
    ALERT,
    "alert",
    line!(
        "M10.3 4.2 2.9 17.3A2 2 0 0 0 4.6 20.3h14.8a2 2 0 0 0 1.7-3L13.7 4.2a2 2 0 0 0-3.4 0Z",
        "M12 9.5v4",
        "M12 17h.01"
    )
);
icon!(
    CHECK_CIRCLE,
    "check-circle",
    concat!(ring!("12", "12", "9"), line!("M8.5 12.2l2.4 2.3 4.6-4.8"))
);
icon!(
    CHECK_ALL,
    "check-all",
    line!("M2.5 12.5l4.5 4.5 9.5-10", "M12 16.5l.5.5 9.5-10")
);
icon!(
    SEARCH_X,
    "search-x",
    concat!(
        ring!("10.5", "10.5", "6.5"),
        line!("M20.5 20.5l-5.4-5.4", "M8.5 8.5l4 4", "M12.5 8.5l-4 4")
    )
);
icon!(PLUS, "plus", line!("M12 5v14", "M5 12h14"));
icon!(CLOSE, "close", line!("M6.5 6.5l11 11", "M17.5 6.5l-11 11"));
icon!(
    BELL,
    "bell",
    line!(
        "M6 9.5a6 6 0 0 1 12 0c0 6 2.5 7.5 2.5 7.5h-17S6 15.5 6 9.5Z",
        "M10 20.5a2.2 2.2 0 0 0 4 0"
    )
);
icon!(
    TERMINAL,
    "terminal",
    line!("M5 7.5l4.5 4.5L5 16.5", "M12 17h7")
);
icon!(
    DOWNLOAD,
    "download",
    line!("M12 4v11", "M7.5 10.5 12 15l4.5-4.5", "M4.5 19.5h15")
);
icon!(
    USER,
    "user",
    concat!(ring!("12", "8.5", "3.5"), line!("M5 20a7 7 0 0 1 14 0"))
);
icon!(
    COPY,
    "copy",
    line!("M9 9h10.5v10.5H9Z", "M15 9V4.5H4.5V15H9")
);
icon!(
    POWER,
    "power",
    line!("M12 3.5v8", "M7 6.2a7.5 7.5 0 1 0 10 0")
);
icon!(
    SEND,
    "send",
    line!("M20.5 3.5 10 14", "M20.5 3.5 14 20.5l-4-6.5-6.5-4Z")
);
icon!(
    ALARM,
    "alarm",
    concat!(
        ring!("12", "13", "7"),
        line!("M12 9.5V13l2.5 1.5", "M4 5.5 6.5 3", "M20 5.5 17.5 3")
    )
);
icon!(
    CROSS_CIRCLE,
    "cross-circle",
    concat!(ring!("12", "12", "9"), line!("M9 9l6 6", "M15 9l-6 6"))
);
icon!(
    CHEVRON_UP_DOWN,
    "chevron-up-down",
    line!("M8.5 9.5 12 6l3.5 3.5", "M8.5 14.5 12 18l3.5-3.5")
);
icon!(
    FOLDER_SEARCH,
    "folder-search",
    concat!(
        line!(
            "M3.5 17.5V6.5A1.5 1.5 0 0 1 5 5h4l2 2.5h8A1.5 1.5 0 0 1 20.5 9v2.5",
            "M3.5 17.5A1.5 1.5 0 0 0 5 19h6",
            "M20.5 20.5l-2-2"
        ),
        ring!("16", "16", "3")
    )
);
