//! Embedded wordlists for information-type generators. Deliberately modest:
//! variety comes from combination, and outputs never claim to be exhaustive
//! demographic data.

pub const FIRST_NAMES: &[&str] = &[
    "James", "Mary", "Robert", "Patricia", "John", "Jennifer", "Michael", "Linda", "David",
    "Elizabeth", "William", "Barbara", "Richard", "Susan", "Joseph", "Jessica", "Thomas", "Sarah",
    "Christopher", "Karen", "Charles", "Lisa", "Daniel", "Nancy", "Matthew", "Betty", "Anthony",
    "Sandra", "Mark", "Margaret", "Donald", "Ashley", "Steven", "Kimberly", "Andrew", "Emily",
    "Paul", "Donna", "Joshua", "Michelle", "Kenneth", "Carol", "Kevin", "Amanda", "Brian",
    "Melissa", "George", "Deborah", "Timothy", "Stephanie", "Ronald", "Rebecca", "Jason", "Sharon",
    "Edward", "Laura", "Jeffrey", "Cynthia", "Ryan", "Dorothy", "Jacob", "Amy", "Gary", "Kathleen",
    "Nicholas", "Angela", "Eric", "Shirley", "Jonathan", "Emma", "Stephen", "Brenda", "Larry",
    "Pamela", "Justin", "Nicole", "Scott", "Anna", "Brandon", "Samantha", "Benjamin", "Katherine",
    "Samuel", "Christine", "Gregory", "Debra", "Alexander", "Rachel", "Patrick", "Carolyn",
    "Frank", "Janet", "Raymond", "Maria", "Jack", "Olivia", "Dennis", "Heather", "Jerry", "Helen",
    "Tyler", "Catherine", "Aaron", "Diane", "Jose", "Julie", "Adam", "Victoria", "Nathan", "Joyce",
    "Henry", "Lauren", "Zachary", "Kelly", "Douglas", "Christina", "Peter", "Ruth", "Kyle", "Joan",
    "Noah", "Virginia", "Ethan", "Judith", "Jeremy", "Evelyn", "Walter", "Hannah", "Christian",
    "Andrea", "Keith", "Megan", "Roger", "Cheryl", "Terry", "Jacqueline", "Austin", "Madison",
    "Sean", "Teresa", "Gerald", "Abigail", "Carl", "Sophia", "Harold", "Martha", "Dylan", "Sara",
    "Arthur", "Gloria", "Lawrence", "Janice", "Jordan", "Kathryn", "Jesse", "Ann", "Bryan", "Isabella",
    "Billy", "Judy", "Bruce", "Charlotte", "Gabriel", "Julia", "Joe", "Grace", "Logan", "Amber",
    "Alan", "Alice", "Juan", "Jean", "Albert", "Denise", "Willie", "Frances", "Elijah", "Danielle",
    "Wayne", "Marilyn", "Randy", "Natalie", "Vincent", "Beverly", "Mason", "Diana", "Roy", "Brittany",
    "Ralph", "Theresa", "Bobby", "Kayla", "Russell", "Alexis", "Bradley", "Doris", "Philip", "Lori",
    "Eugene", "Tiffany",
];

pub const LAST_NAMES: &[&str] = &[
    "Smith", "Johnson", "Williams", "Brown", "Jones", "Garcia", "Miller", "Davis", "Rodriguez",
    "Martinez", "Hernandez", "Lopez", "Gonzalez", "Wilson", "Anderson", "Thomas", "Taylor",
    "Moore", "Jackson", "Martin", "Lee", "Perez", "Thompson", "White", "Harris", "Sanchez",
    "Clark", "Ramirez", "Lewis", "Robinson", "Walker", "Young", "Allen", "King", "Wright",
    "Scott", "Torres", "Nguyen", "Hill", "Flores", "Green", "Adams", "Nelson", "Baker", "Hall",
    "Rivera", "Campbell", "Mitchell", "Carter", "Roberts", "Gomez", "Phillips", "Evans", "Turner",
    "Diaz", "Parker", "Cruz", "Edwards", "Collins", "Reyes", "Stewart", "Morris", "Morales",
    "Murphy", "Cook", "Rogers", "Gutierrez", "Ortiz", "Morgan", "Cooper", "Peterson", "Bailey",
    "Reed", "Kelly", "Howard", "Ramos", "Kim", "Cox", "Ward", "Richardson", "Watson", "Brooks",
    "Chavez", "Wood", "James", "Bennett", "Gray", "Mendoza", "Ruiz", "Hughes", "Price", "Alvarez",
    "Castillo", "Sanders", "Patel", "Myers", "Long", "Ross", "Foster", "Jimenez", "Powell",
    "Jenkins", "Perry", "Russell", "Sullivan", "Bell", "Coleman", "Butler", "Henderson", "Barnes",
    "Gonzales", "Fisher", "Vasquez", "Simmons", "Romero", "Jordan", "Patterson", "Alexander",
    "Hamilton", "Graham", "Reynolds", "Griffin", "Wallace", "Moreno", "West", "Cole", "Hayes",
    "Bryant", "Herrera", "Gibson", "Ellis", "Tran", "Medina", "Aguilar", "Stevens", "Murray",
    "Ford", "Castro", "Marshall", "Owens", "Harrison", "Fernandez", "McDonald", "Woods",
    "Washington", "Kennedy", "Wells", "Vargas", "Henry", "Chen", "Freeman", "Webb", "Tucker",
    "Guzman", "Burns", "Crawford", "Olson", "Simpson", "Porter", "Hunter", "Gordon", "Mendez",
    "Silva", "Shaw", "Snyder", "Mason", "Dixon", "Munoz", "Hunt", "Hicks", "Holmes", "Palmer",
    "Wagner", "Black", "Robertson", "Boyd", "Rose", "Stone", "Salazar", "Fox", "Warren", "Mills",
    "Meyer", "Rice", "Schmidt", "Garza", "Daniels", "Ferguson", "Nichols", "Stephens", "Soto",
    "Weaver", "Ryan",
];

pub const STREET_NAMES: &[&str] = &[
    "Maple", "Oak", "Cedar", "Pine", "Elm", "Washington", "Lake", "Hill", "Walnut", "Spring",
    "North", "Ridge", "Church", "Willow", "Mill", "Sunset", "Railroad", "Jackson", "West", "South",
    "Center", "Highland", "Forest", "River", "Meadow", "Franklin", "Chestnut", "Park", "Main",
    "Prospect", "Broad", "Madison", "Pearl", "Locust", "Grove", "Cherry", "Spruce", "Adams",
    "Jefferson", "Lincoln", "Dogwood", "Hickory", "Magnolia", "Sycamore", "Valley", "Summit",
    "Union", "Liberty", "Harrison", "Monroe",
];

pub const STREET_SUFFIXES: &[&str] =
    &["St", "Ave", "Blvd", "Dr", "Ln", "Rd", "Way", "Ct", "Pl", "Ter"];

/// (city, state code, zip prefix). Zip prefixes are real-ish 3-digit area
/// prefixes so generated zips look plausible for the city.
pub const CITIES: &[(&str, &str, &str)] = &[
    ("Springfield", "IL", "627"),
    ("Riverton", "WY", "825"),
    ("Fairview", "OR", "970"),
    ("Franklin", "TN", "370"),
    ("Greenville", "SC", "296"),
    ("Bristol", "CT", "060"),
    ("Clinton", "IA", "527"),
    ("Salem", "OR", "973"),
    ("Madison", "WI", "537"),
    ("Georgetown", "TX", "786"),
    ("Arlington", "VA", "222"),
    ("Ashland", "OH", "448"),
    ("Burlington", "VT", "054"),
    ("Manchester", "NH", "031"),
    ("Milton", "FL", "325"),
    ("Newport", "RI", "028"),
    ("Oxford", "MS", "386"),
    ("Dayton", "OH", "454"),
    ("Lexington", "KY", "405"),
    ("Auburn", "AL", "368"),
    ("Boulder", "CO", "803"),
    ("Chandler", "AZ", "852"),
    ("Dover", "DE", "199"),
    ("Eugene", "OR", "974"),
    ("Fremont", "CA", "945"),
    ("Gilbert", "AZ", "852"),
    ("Hudson", "NY", "125"),
    ("Irvine", "CA", "926"),
    ("Joliet", "IL", "604"),
    ("Kirkland", "WA", "980"),
    ("Lansing", "MI", "489"),
    ("Mesa", "AZ", "852"),
    ("Norfolk", "VA", "235"),
    ("Olympia", "WA", "985"),
    ("Plano", "TX", "750"),
    ("Quincy", "MA", "021"),
    ("Raleigh", "NC", "276"),
    ("Syracuse", "NY", "132"),
    ("Tacoma", "WA", "984"),
    ("Utica", "NY", "135"),
    ("Vancouver", "WA", "986"),
    ("Waco", "TX", "767"),
    ("Yonkers", "NY", "107"),
    ("Albany", "NY", "122"),
    ("Bend", "OR", "977"),
    ("Carmel", "IN", "460"),
    ("Denton", "TX", "762"),
    ("Edison", "NJ", "088"),
    ("Fargo", "ND", "581"),
    ("Provo", "UT", "846"),
];

pub const BUSINESS_HEADS: &[&str] = &[
    "Apex", "Summit", "Pioneer", "Cascade", "Vertex", "Horizon", "Beacon", "Keystone", "Sterling",
    "Granite", "Harbor", "Crestline", "Bluepeak", "Ironwood", "Silverline", "Northwind", "Redwood",
    "Clearwater", "Stonebridge", "Brightpath", "Copperfield", "Lakeshore", "Meridian", "Pinnacle",
    "Quantum", "Solstice", "Trailhead", "Union", "Vantage", "Westgate",
];

pub const BUSINESS_MIDS: &[&str] = &[
    "Data", "Logic", "Cloud", "Micro", "Digital", "Global", "Dynamic", "Integrated", "Advanced",
    "Precision", "Unified", "Strategic", "Creative", "Reliable", "Innovative", "Applied",
];

pub const BUSINESS_TAILS: &[&str] = &[
    "Systems", "Solutions", "Industries", "Group", "Partners", "Labs", "Works", "Technologies",
    "Consulting", "Holdings", "Ventures", "Enterprises", "Associates", "Corporation", "Company", "LLC",
];

pub const EMAIL_DOMAINS: &[&str] = &[
    "example.com", "example.org", "example.net", "mailinator.example", "inbox.example",
    "post.example", "mail.example",
];

pub const HOST_WORDS: &[&str] = &[
    "alpha", "bravo", "cedar", "delta", "ember", "flint", "grove", "harbor", "indigo", "juniper",
    "krypton", "lumen", "mesa", "nimbus", "onyx", "pluto", "quartz", "raven", "sierra", "topaz",
    "umber", "vortex", "willow", "xenon", "yarrow", "zephyr",
];

pub const TLDS: &[&str] = &["example", "test", "invalid"];

pub const FILE_WORDS: &[&str] = &[
    "report", "invoice", "summary", "notes", "draft", "final", "backup", "export", "archive",
    "statement", "budget", "roster", "plan", "outline", "review", "brief", "memo", "log",
];

pub const URL_PATH_WORDS: &[&str] = &[
    "docs", "assets", "images", "files", "posts", "articles", "products", "users", "orders",
    "reports", "static", "media", "public", "content", "pages",
];

